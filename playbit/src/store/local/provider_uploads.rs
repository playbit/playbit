//! Replica-local AP durability receipts. These never qualify namespace facts,
//! grant authority, or enter replicated roots. An old receipt cannot clear a
//! newer local edit: items compare their exact immutable selected event IDs.
use super::*;
use crate::store::{EntryId, volume_index};
use volume_index::namespace::MAX_FACTS;

fn receipt_key(volume: Id, event: EntryId) -> String {
    format!("provider-uploaded/{volume}/{event}")
}
fn revision_key(volume: Id) -> String {
    format!("provider-upload-revision/{volume}")
}
fn policy_key(volume: Id) -> String {
    format!("provider-policy-uploaded/{volume}")
}
fn policy_fingerprint(clock: kvdb::Clock, bytes: &[u8]) -> [u8; 32] {
    let mut hash = blake3::Hasher::new();
    hash.update(&clock.as_u64().to_le_bytes());
    hash.update(bytes);
    *hash.finalize().as_bytes()
}

impl LocalStore {
    fn provider_policy_fingerprint(&self, volume: Id) -> Result<Option<[u8; 32]>> {
        Ok(self
            .db(volume, crate::store::DatabaseId::Index)?
            .begin_read()?
            .get_with_clock(&VOLUME_KEY)?
            .map(|(clock, bytes)| policy_fingerprint(clock, &bytes)))
    }

    pub(super) fn provider_volume_uploaded(&self, volume: &Volume) -> Result<bool> {
        // The catalog descriptor may predate a concurrent rename. Qualify the
        // displayed label and receipt from one policy cut, not a newer label's
        // acknowledgment applied to an older projected folder.
        let index = self.db(volume.id, crate::store::DatabaseId::Index)?;
        let snapshot = index.begin_read()?;
        let Some((clock, bytes)) = snapshot.get_with_clock(&VOLUME_KEY)? else { return Ok(false) };
        if record::decode::<Volume>(&bytes)? != *volume {
            return Ok(false);
        }
        let current = policy_fingerprint(clock, &bytes);
        let acknowledged: Option<[u8; 32]> = get(&self.metadata, policy_key(volume.id).as_bytes())?;
        Ok(acknowledged == Some(current))
    }

    #[cfg(any(test, feature = "file-provider"))]
    pub(super) fn provider_upload_revision(&self, volume: Id) -> Result<u64> {
        Ok(get(&self.metadata, revision_key(volume).as_bytes())?.unwrap_or(0))
    }

    /// Caller holds the namespace and profile guards. All body/namespace
    /// receipts and their change anchor commit in the same metadata transaction.
    pub(super) fn advance_provider_upload(
        tx: &mut kvdb::WriteTxn<'_, RedbStorage>,
        volume: Id,
    ) -> Result<()> {
        let key = revision_key(volume);
        let revision =
            tx.get(key.as_bytes())?.map(|bytes| decode::<u64>(&bytes)).transpose()?.unwrap_or(0);
        let next = revision.checked_add(1).ok_or(Error::LimitExceeded)?;
        tx.put(key.as_bytes(), &encode(&next)?)?;
        Ok(())
    }

    fn record_provider_uploads(
        &self,
        volume: Id,
        policy: Option<[u8; 32]>,
        visit: impl FnOnce(&mut dyn FnMut(EntryId) -> Result<()>) -> Result<()>,
    ) -> Result<bool> {
        // Ack only the current clock and body. A delayed older acknowledgment
        // cannot qualify a later rename or replace a newer policy receipt.
        let policy = match policy {
            Some(policy) => {
                (self.provider_policy_fingerprint(volume)? == Some(policy)).then_some(policy)
            }
            None => None,
        };
        let _profile = self.lock_profile()?;
        let mut tx = self.metadata.begin_write(now_unix_secs())?;
        let count_key = format!("provider-upload-count/{volume}");
        let mut count = tx
            .get(count_key.as_bytes())?
            .map(|bytes| decode::<usize>(&bytes))
            .transpose()?
            .unwrap_or(0);
        if count > MAX_FACTS {
            return Err(Error::corrupt("provider upload receipt count exceeds namespace limit"));
        }
        let mut changed = false;
        if let Some(policy) = policy {
            let key = policy_key(volume);
            let previous =
                tx.get(key.as_bytes())?.map(|bytes| decode::<[u8; 32]>(&bytes)).transpose()?;
            if previous != Some(policy) {
                tx.put(key.as_bytes(), &encode(&policy)?)?;
                changed = true;
            }
        }
        visit(&mut |event| {
            let key = receipt_key(volume, event);
            if let Some(bytes) = tx.get(key.as_bytes())? {
                if *bytes != [1] {
                    return Err(Error::corrupt("invalid provider upload receipt"));
                }
                return Ok(());
            }
            if count >= MAX_FACTS {
                return Err(Error::LimitExceeded);
            }
            tx.put(key.as_bytes(), &[1])?;
            count += 1;
            changed = true;
            Ok(())
        })?;
        if changed {
            tx.put(count_key.as_bytes(), &encode(&count)?)?;
            Self::advance_provider_upload(&mut tx, volume)?;
            tx.commit()?;
            #[cfg(any(test, feature = "file-provider"))]
            self.changes.provider_status_changed(volume);
        }
        Ok(changed)
    }

    /// Trusted transport only: call after AP acknowledged these exact bytes,
    /// or after validating and durably applying this authenticated AP event.
    #[cfg(feature = "connected-engine")]
    pub(crate) fn acknowledge_provider_delta(&self, volume: Id, bytes: &[u8]) -> Result<bool> {
        let operations = self.operations(&[volume])?;
        let _guards = operations.lock()?;
        if !self
            .personal_volumes()?
            .iter()
            .any(|item| item.id == volume && item.share_key_with_service)
        {
            return Ok(false);
        }
        let mut policy = None;
        kvdb::sync::visit_delta(bytes, crate::store::MAX_REPLICA_DELTA_BYTES, |key, state| {
            if key == VOLUME_KEY
                && let kvdb::sync::EntState::Present { clock, value } = state
            {
                policy = Some(policy_fingerprint(clock, value));
            }
        })?;
        self.record_provider_uploads(volume, policy, |record| {
            let mut result = Ok(());
            kvdb::sync::visit_delta(bytes, crate::store::MAX_REPLICA_DELTA_BYTES, |key, state| {
                if result.is_err() {
                    return;
                }
                if let kvdb::sync::EntState::Present { clock, value } = state {
                    result = volume_index::acknowledged_namespace_event(key, clock, value)
                        .and_then(|event| event.map_or(Ok(()), &mut *record));
                }
            })?;
            result
        })
    }

    /// An initialization receipt can seed the exact current service cut. A
    /// concurrent local edit leaves the cut pending rather than guessing that
    /// an older receipt includes it. No pull or network request occurs here.
    pub(super) fn acknowledge_provider_snapshot(
        &self,
        volume: Id,
        root: ReplicaRoot,
    ) -> Result<()> {
        let operations = self.operations(&[volume])?;
        let _guards = operations.lock()?;
        if !self.personal_volumes()?.iter().any(|item| item.id == volume) {
            return Ok(());
        }
        let index = self.db(volume, crate::store::DatabaseId::Index)?;
        let policy = DatabasePolicy::read(&index, crate::store::DatabaseId::Index)?;
        if !policy.volume.volume().share_key_with_service {
            return Ok(());
        }
        if projection_root(&index, &policy, ReplicaReader::Service)? != root {
            return Ok(());
        }
        self.provider_snapshot_receipts(volume, &index, root)?;
        Ok(())
    }

    /// Initialization retains the exact received cut even if another local
    /// edit already advanced the published index. Never seed from that newer cut.
    #[cfg(feature = "connected-engine")]
    pub(crate) fn acknowledge_provider_received(
        &self,
        volume: Id,
        source: &Database,
        root: ReplicaRoot,
    ) -> Result<()> {
        let operations = self.operations(&[volume])?;
        let _guards = operations.lock()?;
        if !self
            .personal_volumes()?
            .iter()
            .any(|item| item.id == volume && item.share_key_with_service)
        {
            return Ok(());
        }
        self.provider_snapshot_receipts(volume, source, root)
    }

    fn provider_snapshot_receipts(
        &self,
        volume: Id,
        source: &Database,
        root: ReplicaRoot,
    ) -> Result<()> {
        let read = source.begin_read()?;
        if read.root_hash()? != root {
            return Ok(());
        }
        let policy = read
            .get_with_clock(&VOLUME_KEY)?
            .map(|(clock, bytes)| policy_fingerprint(clock, &bytes));
        self.record_provider_uploads(volume, policy, |record| {
            for row in read.iter() {
                let (key, _) = row?;
                if let Some((clock, bytes)) = read.get_with_clock(&key)?
                    && let Some(event) =
                        volume_index::acknowledged_namespace_event(&key, clock, &bytes)?
                {
                    record(event)?
                }
            }
            Ok(())
        })?;
        Ok(())
    }

    pub(super) fn provider_item_uploaded(
        &self,
        volume: Id,
        snapshot: &volume_index::IndexSnapshot<'_, RedbStorage>,
        policy: &MetadataPolicy,
        entry: &volume_index::VolumeEntry,
    ) -> Result<bool> {
        if !policy.volume().share_key_with_service {
            return Ok(false);
        }
        let receipts = self.metadata.begin_read()?;
        for event in snapshot.provider_sources(entry.id)?.into_iter().flatten() {
            let Some(bytes) = receipts.get(receipt_key(volume, event).as_bytes())? else {
                return Ok(false);
            };
            if *bytes != [1] {
                return Err(Error::corrupt("invalid provider upload receipt"));
            }
        }
        if let volume_index::EntryKind::Blob { len_bytes, digest, encrypted_key, .. } =
            &entry.info.kind
        {
            if !super::entries::granted(snapshot.authorize(
                policy.volume(),
                policy.default_permissions(),
                crate::store::PLAYBIT_REPLICA_PRINCIPAL,
                entry.id,
                Permission::Read,
            ))? {
                return Ok(false);
            }
            let version = volume_index::BlobVersion {
                address: volume_index::EntryAddress { volume, entry: entry.id },
                revision: entry.content_revision,
                len_bytes: *len_bytes,
                digest: *digest,
                encrypted_key: *encrypted_key,
            };
            return self.blob_durable_receipt(&version);
        }
        // .pbdb is a reference, not a serialization of its backing database.
        Ok(true)
    }
}
