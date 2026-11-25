//! One account record schema for native and browser clients. Encryption and
//! local capability conflict checks are separate, required receipt boundaries.
use crate::store::catalog::validate_version;
use crate::store::{Error, Id, Invitation, PersonalVolumeSelection, Result, Volume};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

mod codec;

pub fn validate_display_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > playbit_api::abi::environment::DISPLAY_NAME_BYTES
        || name.trim() != name
        || name.chars().any(char::is_control)
    {
        return Err(Error::InvalidInput(
            "display name must contain 1–256 UTF-8 bytes without surrounding whitespace or control characters",
        ));
    }
    Ok(())
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Credentials {
    pub database_key: [u8; 32],
    pub blob_key: [u8; 32],
}

/// Permanent reason stored for a recipient's explicit decline. Other nonempty
/// reason codes still hide a volume without acquiring this reason's meaning.
pub const DECLINE_INVITATION: &str = "decline_invitation";

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[allow(
    clippy::large_enum_variant,
    reason = "Account records keep bounded decoded values inline without a second allocation."
)]
pub enum Record {
    VolumeVisibility {
        volume: Id,
        hidden_reason: Option<String>,
    },
    AppVisibility {
        app: crate::package::AppIdentity,
        removed: bool,
    },
    DisplayName {
        name: String,
    },
    Volume {
        volume: Volume,
        credentials: Credentials,
    },
    VolumeDeleted {
        volume: Id,
        #[serde(skip_serializing_if = "Option::is_none")]
        withdrawal_owner: Option<Id>,
    },
    Invitation {
        invitation: Invitation,
        app_name: Option<String>,
    },
    PersonalVolumes {
        session: String,
        grants: Vec<PersonalVolumeSelection>,
    },
    HostedVolume {
        volume: Volume,
        credentials: Credentials,
        scope: crate::store::AppScope,
    },
    HostedInvitation {
        invitation: Invitation,
        app_name: Option<String>,
        scope: crate::store::AppScope,
    },
}
impl Record {
    pub fn decode(plain: &[u8]) -> Result<Self> {
        super::format::Frame::decode(plain)?
            .admit()?
            .ok_or(Error::InvalidInput("optional account record has no known projection"))
    }

    pub fn encode(&self) -> Result<Vec<u8>> {
        super::format::Frame::encode(self, None)
    }

    pub fn rewrite(&self, original: &[u8]) -> Result<Vec<u8>> {
        super::format::Frame::encode(self, Some(original))
    }

    pub(super) fn validate(&self) -> Result<()> {
        match self {
            Self::AppVisibility { app, .. } => app.validate().map_err(Error::InvalidInput)?,
            Self::DisplayName { name } => validate_display_name(name)?,
            Self::Volume { volume, credentials }
            | Self::HostedVolume { volume, credentials, .. } => {
                if volume.name.trim().is_empty() || credentials.database_key == credentials.blob_key
                {
                    return Err(Error::InvalidInput("invalid account volume"));
                }
            }
            Self::PersonalVolumes { session, grants } => {
                validate_version(session)?;
                let mut seen = BTreeSet::new();
                let mut incarnations = BTreeSet::new();
                if grants.len() > playbit_api::abi::syscall::MAX_VOLUMES - 1
                    || grants.iter().any(|grant| {
                        !seen.insert(grant.volume)
                            || grant.incarnation == crate::store::Id::from_bytes([0; 16])
                            || !incarnations.insert(grant.incarnation)
                    })
                {
                    return Err(Error::InvalidInput("invalid account personal volume selection"));
                }
            }
            Self::Invitation { invitation, app_name }
            | Self::HostedInvitation { invitation, app_name, .. } => {
                invitation.validate()?;
                if app_name.as_ref().is_some_and(|name| name.is_empty() || name.len() > 256) {
                    return Err(Error::InvalidInput("invalid app name"));
                }
            }
            Self::VolumeVisibility { hidden_reason, .. } => {
                if hidden_reason.as_ref().is_some_and(|reason| {
                    reason.is_empty()
                        || reason.len() > 64
                        || !reason.bytes().all(|byte| {
                            byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_'
                        })
                }) {
                    return Err(Error::InvalidInput("invalid hidden volume reason"));
                }
            }
            Self::VolumeDeleted { .. } => {}
        }
        self.validate_hosted_scope()
    }

    fn validate_hosted_scope(&self) -> Result<()> {
        match self {
            Self::HostedVolume { volume, scope, .. } => {
                scope.validate()?;
                let crate::store::AppScope::World { app, .. } = scope else {
                    return Err(Error::InvalidInput("hosted volume requires World metadata"));
                };
                if volume.owner != app.publisher
                    || volume.personal
                    || !volume.share_key_with_service
                    || volume.id
                        != app
                            .volume_id(crate::package::AutomaticVolumeRole::Root, None, None, None)
                            .map_err(Error::InvalidInput)?
                {
                    return Err(Error::InvalidInput("hosted World identity differs"));
                }
            }
            Self::HostedInvitation { invitation, scope, .. } => {
                scope.validate()?;
                if !matches!(scope, crate::store::AppScope::Session { id, .. } if id == &invitation.session.id)
                {
                    return Err(Error::InvalidInput("hosted session identity differs"));
                }
            }
            _ => {}
        }
        Ok(())
    }

    pub(crate) fn logical_key(&self) -> Result<String> {
        Ok(match self {
            Self::VolumeVisibility { volume, .. } => format!("optional/volume_visibility/{volume}"),
            Self::AppVisibility { app, .. } => {
                format!("optional/app_visibility/{}/{}", app.publisher, app.app_id)
            }
            Self::DisplayName { .. } => "settings/display-name".into(),
            Self::Volume { volume, .. } | Self::HostedVolume { volume, .. } => {
                format!("volume/{}", volume.id)
            }
            Self::VolumeDeleted { volume, .. } => format!("deleted-volume/{volume}"),
            Self::Invitation { invitation, .. } | Self::HostedInvitation { invitation, .. } => {
                format!("session/{}", invitation.session.id)
            }
            Self::PersonalVolumes { session, .. } => {
                validate_version(session)?;
                format!("session-mounts/{session}")
            }
        })
    }
    pub(super) fn kind(&self) -> &'static str {
        match self {
            Self::VolumeVisibility { .. } => "volume_visibility",
            Self::AppVisibility { .. } => "app_visibility",
            Self::DisplayName { .. } => "display_name",
            Self::HostedVolume { .. } => "hosted_volume",
            Self::HostedInvitation { .. } => "hosted_invitation",
            Self::Volume { .. } => "volume",
            Self::VolumeDeleted { .. } => "volume_deleted",
            Self::Invitation { .. } => "invitation",
            Self::PersonalVolumes { .. } => "personal_volumes",
        }
    }

    /// The browser authenticates and binds the logical key before interpreting
    /// the admission status or projecting this bounded JSON view.
    pub fn browser_plaintext(plain: &[u8]) -> Result<Vec<u8>> {
        let decoded = super::format::Frame::decode(plain)?;
        let mut output = Vec::with_capacity(plain.len() + 256);
        serde_json::to_writer(&mut output, &decoded)
            .map_err(|_| Error::Internal("could not encode an account record"))?;
        if output.len() > super::MAX_PLAINTEXT_BYTES * 4 + 256 {
            return Err(Error::LimitExceeded);
        }
        Ok(output)
    }
}
