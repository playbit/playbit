//! A local display capability. Remote session identity never contains this seed.
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
