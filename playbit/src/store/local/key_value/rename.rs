use super::*;

impl LocalStore {
    /// Rename from one exact admitted read cut without overwriting another key.
    pub(crate) fn kv_rename(
        &self,
        access: &Access,
        address: EntryAddress,
        old_key: &[u8],
        new_key: &[u8],
        expected: key_value::Root,
        value: &[u8],
    ) -> Result<key_value::Root> {
        key_value::validate_key(old_key)?;
        key_value::validate_key(new_key)?;
        if old_key == new_key {
            return Err(Error::InvalidInput("KV rename requires a different key"));
        }
        if value.len() > key_value::MAX_BYTES {
            return Err(Error::LimitExceeded);
        }
        let operations = self.access_operations(access, address.volume)?;
        let _guards = operations.lock()?;
        self.kv_backing(access, address, Permission::Read)?;
        let database = self.kv_backing(access, address, Permission::Write)?;
        let mut tx = database.begin_write_with_delta(now_unix_secs(), MAX_REPLICA_DELTA_BYTES)?;
        if tx.root_hash()?.map(key_value::Root::from) != Some(expected) {
            return Err(Error::LocalContention("KV source changed before rename"));
        }
        if !tx.has(old_key)? {
            return Err(Error::NotFound("original KV key"));
        }
        if tx.has(new_key)? {
            return Err(Error::AlreadyExists("destination KV key"));
        }
        tx.put(new_key, value)?;
        tx.del(old_key)?;
        let root = tx
            .root_hash()?
            .map(key_value::Root::from)
            .ok_or(Error::Internal("KV rename produced no source root"))?;
        let delta = tx.commit_with_delta()?;
        self.changes.publish(address.volume, DatabaseId::Entry(address.entry), delta);
        Ok(root)
    }
}

#[cfg(test)]
mod tests;
