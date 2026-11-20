//! Account-encrypted scope bindings accompany ordinary volume and session capabilities.
use super::{Error, Id, Result, catalog::validate_version};
use crate::package::AppIdentity;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum AppScope {
    World { app: AppIdentity, build: String, initialization: [u8; 32] },
    Session { id: String, creation_build: String, user_volume: Option<Id> },
}

impl AppScope {
    pub(crate) fn validate(&self) -> Result<()> {
        match self {
            Self::World { app, build, .. } => {
                app.validate().map_err(Error::InvalidInput)?;
                validate_version(build)?;
            }
            Self::Session { id, creation_build, .. } => {
                validate_version(id)?;
                validate_version(creation_build)?;
            }
        }
        Ok(())
    }

    #[cfg(all(not(target_arch = "wasm32"), feature = "local-store"))]
    pub(crate) fn logical_key(&self) -> Result<String> {
        use crate::package::AutomaticVolumeRole;

        self.validate()?;
        Ok(match self {
            Self::World { app, .. } => format!(
                "automatic-world/{}",
                app.volume_id(AutomaticVolumeRole::Root, None, None, None)
                    .map_err(Error::InvalidInput)?
            ),
            Self::Session { id, .. } => format!("automatic-session/{id}"),
        })
    }
}
