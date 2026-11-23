//! A local invitation reference for sharing and guest admission. The AP addresses sessions by public ID.
use super::{Error, Result, Seed};
use crate::release_channel::ReleaseChannel;
use serde::Serialize;

#[derive(Clone)]
#[cfg_attr(feature = "app-host", derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize))]
pub struct SessionReference {
    id: String,
    seed: Seed,
}
impl std::fmt::Debug for SessionReference {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionReference").field("id", &self.id).finish_non_exhaustive()
    }
}
impl SessionReference {
    pub fn new(seed: Seed) -> Result<Self> {
        Ok(Self { id: super::session_id(&seed)?, seed })
    }
    /// Only a retained session can supply a guest's invitation credential.
    #[cfg(all(
        feature = "connected-engine",
        any(feature = "native-runtime", feature = "wasm-runtime")
    ))]
    pub(crate) fn retained(store: &super::LocalStore, id: &str) -> Result<Self> {
        let reference = Self::new(store.session_seed(id)?)?;
        if reference.internal_id() != id {
            return Err(Error::corrupt("retained invitation differs from the session identity"));
        }
        Ok(reference)
    }
    #[cfg(all(
        feature = "connected-engine",
        any(feature = "native-runtime", feature = "wasm-runtime")
    ))]
    pub(crate) fn key_secret(&self) -> Seed {
        self.seed
    }
    pub fn from_link(value: &str, channel: ReleaseChannel) -> Result<Self> {
        Self::new(super::parse_session_link(value, channel)?)
    }
    pub fn internal_id(&self) -> &str {
        &self.id
    }
    pub fn code(&self) -> String {
        super::session_code(&self.seed)
    }
    pub fn link(&self, channel: ReleaseChannel) -> String {
        super::session_link(&self.seed, channel)
    }
    pub fn validate(&self) -> Result<()> {
        if super::session_id(&self.seed)? != self.id {
            return Err(Error::InvalidInput("session reference identity differs"));
        }
        Ok(())
    }
}

impl Serialize for SessionReference {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.code())
    }
}
