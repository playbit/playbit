//! Durable admission for account-provider publication. A marker precedes local
//! mutations; clearing it requires the same generation after a completed drain.
//! False-positive work is safe, while absent intent could strand offline edits.
use super::*;
use crate::store::file_provider::PROVIDER_VOLUME_LIMIT;

const PREFIX: &[u8] = b"provider-publication/";
const GENERATION: &[u8] = b"provider-publication-generation";

fn pending(metadata: &Database) -> Result<Vec<(Id, u64)>> {
    let mut result = Vec::new();
    for row in metadata.begin_read()?.iter_from(PREFIX) {
        let (key, bytes) = row?;
        let Some(id) = key.strip_prefix(PREFIX) else { break };
        if result.len() == PROVIDER_VOLUME_LIMIT {
            return Err(Error::LimitExceeded);
        }
        let text = std::str::from_utf8(id)
            .map_err(|_| Error::corrupt("invalid provider publication volume identity"))?;
        let generation = decode::<u64>(&bytes)?;
        if generation == 0 {
            return Err(Error::corrupt("invalid provider publication generation"));
        }
        result.push((Id::parse(text)?, generation));
    }
    Ok(result)
}

impl LocalStore {
    /// Call before dispatching a mutation; an uncertain or rejected mutation
    /// may retain extra work, but can never erase a previously committed edit.
    pub fn provider_enqueue_publication(&self, volume: Id) -> Result<()> {
        let _authority = self.lock_account_authority()?;
        let _profile = self.lock_profile()?;
        if !self.personal_volumes()?.iter().any(|item| item.id == volume) {
            return Err(Error::AccessDenied("File Provider personal volume"));
        }
        let mut tx = self.metadata.begin_write(now_unix_secs())?;
        self.queue_provider_publication(&mut tx, volume)?;
        tx.commit()?;
        Ok(())
    }

    /// Caller owns the profile lock. New-volume publication uses this same
    /// transaction so its initialized namespace and admission become visible
    /// together, including across an interrupted native completion.
    pub(super) fn queue_provider_publication(
        &self,
        tx: &mut kvdb::WriteTxn<'_, RedbStorage>,
        volume: Id,
    ) -> Result<()> {
        let key = format!("provider-publication/{volume}");
        if !tx.has(key.as_bytes())? && pending(&self.metadata)?.len() == PROVIDER_VOLUME_LIMIT {
            return Err(Error::LimitExceeded);
        }
        let generation = tx
            .get(GENERATION)?
            .map(|bytes| decode::<u64>(&bytes))
            .transpose()?
            .unwrap_or(0)
            .checked_add(1)
            .ok_or(Error::LimitExceeded)?;
        tx.put(GENERATION, &encode(&generation)?)?;
        tx.put(key.as_bytes(), &encode(&generation)?)?;
        Ok(())
    }

    pub fn provider_pending_publications(&self) -> Result<Vec<(Id, u64)>> {
        let _authority = self.lock_account_authority()?;
        pending(&self.metadata)
    }

    pub fn provider_ack_publication(&self, volume: Id, generation: u64) -> Result<()> {
        let _authority = self.lock_account_authority()?;
        let _profile = self.lock_profile()?;
        if !self.personal_volumes()?.iter().any(|item| item.id == volume) {
            return Err(Error::AccessDenied("File Provider personal volume"));
        }
        let key = format!("provider-publication/{volume}");
        let mut tx = self.metadata.begin_write(now_unix_secs())?;
        let current = tx.get(key.as_bytes())?.map(|bytes| decode::<u64>(&bytes)).transpose()?;
        if current == Some(generation) {
            tx.del(key.as_bytes())?;
            tx.commit()?;
        }
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert durable publication admission across restart.")]
mod tests {
    use super::*;

    #[test]
    fn pending_provider_publication_survives_restart_and_stale_acknowledgments() {
        // Persist admission before editing, restart, then race later intent
        // against old acknowledgments, including after the queue becomes empty.
        let directory = tempfile::tempdir().unwrap();
        let store = LocalStore::open(directory.path()).unwrap();
        let volume = store.provider_create_volume(Id::generate().unwrap(), "Files", true).unwrap();
        let first = store.provider_pending_publications().unwrap()[0];
        assert_eq!(first.0, volume.id);
        store.provider_enqueue_publication(volume.id).unwrap();
        let second = store.provider_pending_publications().unwrap()[0];
        assert!(second.1 > first.1);
        store.provider_ack_publication(volume.id, first.1).unwrap();
        assert_eq!(store.provider_pending_publications().unwrap(), [second]);
        drop(store);
        let store = LocalStore::open(directory.path()).unwrap();
        assert_eq!(store.provider_pending_publications().unwrap(), [second]);
        store.provider_ack_publication(volume.id, second.1).unwrap();
        assert!(store.provider_pending_publications().unwrap().is_empty());
        store.provider_enqueue_publication(volume.id).unwrap();
        let third = store.provider_pending_publications().unwrap()[0];
        assert!(third.1 > second.1);
        store.provider_ack_publication(volume.id, second.1).unwrap();
        assert_eq!(store.provider_pending_publications().unwrap(), [third]);
        store.delete_volume(volume.id).unwrap();
        assert!(store.provider_ack_publication(volume.id, third.1).is_err());
        assert_eq!(store.provider_pending_publications().unwrap(), [third]);
    }
}
