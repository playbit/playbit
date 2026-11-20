//! Bounded observations of accepted entry source changes for local repair.
use super::*;
#[derive(Default)]
pub struct ReceivedMetadata {
    pub entry_ids: BTreeSet<EntryId>,
}
impl ReceivedMetadata {
    pub fn observe(
        &mut self,
        policy: &MetadataPolicy,
        delta: &kvdb::sync::Delta<'_>,
    ) -> Result<()> {
        if crate::store::volume_index::is_index_key(delta.key) {
            let id = crate::store::volume_index::validate_source(delta)?;
            if self.entry_ids.len() == crate::store::volume_index::MAX_REPAIR_IDS
                && !self.entry_ids.contains(&id)
            {
                return Err(Error::LimitExceeded);
            }
            self.entry_ids.insert(id);
            return Ok(());
        }
        let value = match delta.remote {
            kvdb::sync::EntState::Present { value, .. } => Some(value),
            _ => None,
        };
        validate_metadata_entry(&policy.volume, delta.key, value)
    }
}
