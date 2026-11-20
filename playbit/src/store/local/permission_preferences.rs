//! Non-authorizing, account-scoped management preferences never enter replication.
use super::*;
use crate::store::{EntryId, volume_index::MAX_ENTRY_RECORD_BYTES};
use kvdb::storage::ValueRead;

fn key(account: Id, volume: Id, entry: EntryId) -> String {
    format!("settings/entry-permissions/{account}/{volume}/{entry}")
}

impl LocalStore {
    pub(super) fn configured_permissions(
        &self,
        access: &Access,
        volume: Id,
        entry: EntryId,
        active: &EntryAcl,
        manage_permissions: bool,
    ) -> Result<Option<Acl>> {
        match active {
            EntryAcl::Replace(acl) => Ok(Some(acl.clone())),
            EntryAcl::Inherit if manage_permissions => {
                self.retained_entry_permissions(access.account, volume, entry)
            }
            EntryAcl::Inherit => Ok(None),
        }
    }

    pub(super) fn retained_entry_permissions(
        &self,
        account: Id,
        volume: Id,
        entry: EntryId,
    ) -> Result<Option<Acl>> {
        let tx = self.metadata.begin_read()?;
        let Some(reader) = tx.value_reader(key(account, volume, entry).as_bytes())? else {
            return Ok(None);
        };
        if reader.len() > MAX_ENTRY_RECORD_BYTES {
            return Err(Error::LimitExceeded);
        }
        let mut bytes = vec![0; reader.len()];
        reader.read_exact_at(0, &mut bytes)?;
        let acl: Acl = record::decode(&bytes)?;
        acl.validate()?;
        Ok(Some(acl))
    }

    /// The caller holds current account authority and the volume operation gate.
    pub(super) fn remember_entry_permissions(
        &self,
        account: Id,
        volume: Id,
        entry: EntryId,
        acl: &Acl,
    ) -> Result<()> {
        acl.validate()?;
        let encoded = record::encode(acl)?;
        if encoded.len() > MAX_ENTRY_RECORD_BYTES {
            return Err(Error::LimitExceeded);
        }
        let key = key(account, volume, entry);
        let _profile = self.lock_profile()?;
        let mut tx = self.metadata.begin_write(now_unix_secs())?;
        let bytes = {
            let old = tx.get(key.as_bytes())?;
            if old.as_ref().is_some_and(|bytes| bytes.len() > MAX_ENTRY_RECORD_BYTES) {
                return Err(Error::LimitExceeded);
            }
            match old {
                Some(old) => record::rewrite(&old, acl)?,
                None => encoded,
            }
        };
        if bytes.len() > MAX_ENTRY_RECORD_BYTES {
            return Err(Error::LimitExceeded);
        }
        tx.put(key.as_bytes(), &bytes)?;
        tx.commit()?;
        Ok(())
    }
}
