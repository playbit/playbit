//! Durable personal-volume consent. Local owners retain their captured ceiling.
use super::*;
use crate::store::PersonalVolumeGrant;

impl Engine {
    pub(super) fn select_personal_volumes(
        &mut self,
        session: &str,
        grants: Vec<PersonalVolumeGrant>,
    ) -> Result<()> {
        let _timing = crate::trace::Span::new("engine.personal.select");
        self.services.store.select_personal_volumes(session, grants)?;
        // Scoped store history and instance events deliver the actual commit.
        // Current authority fences removed scopes without a network ACK.
        Ok(())
    }

    pub(super) fn add_personal_volumes(
        &mut self,
        session: &str,
        grants: Vec<PersonalVolumeGrant>,
    ) -> Result<()> {
        let _timing = crate::trace::Span::new("engine.personal.select");
        self.services.store.add_personal_volumes(session, grants)?;
        // The same scoped store history and instance events observe this commit.
        Ok(())
    }
}
