//! By-ID KV admission and publication share the index/backing operation gate.
use super::*;
use crate::store::{
    DatabaseId, EntryType, MAX_REPLICA_DELTA_BYTES, key_value,
    volume_index::{DatabaseProtocol, EntryAddress, IndexSnapshot},
};

#[cfg(any(feature = "app-host", test))]
mod rename;

impl LocalStore {
    fn kv_backing(
        &self,
        access: &Access,
        address: EntryAddress,
        permission: Permission,
    ) -> Result<Arc<Database>> {
        let policy = self.index(address.volume)?;
        self.check_authority(access, address.volume, Some(address.entry))?;
        if self.admit_access(access, &policy)?.is_some_and(|grant| !grant.write)
            && permission != Permission::Read
        {
            return Err(Error::AccessDenied("personal volume is read-only"));
        }
        let index = self.db(address.volume, DatabaseId::Index)?;
        let snapshot = IndexSnapshot::new(index.begin_read()?);
        snapshot.authorize(
            policy.volume(),
            policy.default_permissions(),
            self.data_actor(access, policy.volume())?,
            address.entry,
            permission,
        )?;
        let entry = snapshot.entry_policy(address.entry)?;
        if entry.kind != EntryType::KvDb || entry.protocol != Some(DatabaseProtocol::Kvdb) {
            return Err(Error::InvalidInput("entry is not an ordinary KV database"));
        }
        if !snapshot.backing_ready(address.entry)? {
            return Err(Error::NotFound("KV database content is pending"));
        }
        self.entry_backing(address.volume, address.entry, false)
    }

    pub fn kv_keys(
        &self,
        access: &Access,
        address: EntryAddress,
        after: Option<&[u8]>,
        root: Option<key_value::Root>,
        limit: usize,
    ) -> Result<key_value::Page> {
        let _timing = crate::trace::Span::new("storage.kv_keys");
        let operations = self.access_operations(access, address.volume)?;
        let _guards = operations.lock()?;
        let database = self.kv_backing(access, address, Permission::Read)?;
        key_value::page(&database.begin_read()?, after, root, limit)
    }

    pub fn kv_read(
        &self,
        access: &Access,
        address: EntryAddress,
        key: &[u8],
        offset: u64,
        limit: u32,
        root: Option<key_value::Root>,
    ) -> Result<key_value::Chunk> {
        let _timing = crate::trace::Span::new("storage.kv_read");
        let operations = self.access_operations(access, address.volume)?;
        let _guards = operations.lock()?;
        let database = self.kv_backing(access, address, Permission::Read)?;
        key_value::read(&database.begin_read()?, key, offset, limit, root)
    }

    pub fn kv_put(
        &self,
        access: &Access,
        address: EntryAddress,
        key: &[u8],
        value: &[u8],
    ) -> Result<()> {
        self.kv_publish(access, address, key, value, false)
    }

    /// Create one key without replacing an existing value, even with Write.
    pub fn kv_insert(
        &self,
        access: &Access,
        address: EntryAddress,
        key: &[u8],
        value: &[u8],
    ) -> Result<()> {
        self.kv_publish(access, address, key, value, true)
    }

    fn kv_publish(
        &self,
        access: &Access,
        address: EntryAddress,
        key: &[u8],
        value: &[u8],
        create: bool,
    ) -> Result<()> {
        let _timing = crate::trace::Span::new("storage.kv_put");
        key_value::validate_key(key)?;
        if value.len() > key_value::MAX_BYTES {
            return Err(Error::LimitExceeded);
        }
        let operations = self.access_operations(access, address.volume)?;
        let _guards = operations.lock()?;
        let (database, append) = match self.kv_backing(access, address, Permission::Write) {
            Ok(database) => (database, false),
            Err(Error::AccessDenied(_)) => {
                (self.kv_backing(access, address, Permission::Append)?, true)
            }
            Err(error) => return Err(error),
        };
        {
            let read = database.begin_read()?;
            if let Some(reader) = read.value_reader(key)? {
                if create {
                    return Err(Error::AlreadyExists("KV key"));
                }
                if key_value::identical(&reader, value)? {
                    return Ok(());
                }
                if append {
                    return Err(Error::AccessDenied("Append cannot replace a KV key"));
                }
            } else if append && read.is_del(key)?.is_some() {
                return Err(Error::AccessDenied("Append cannot recreate a deleted KV key"));
            }
        }
        let mut tx = database.begin_write_with_delta(now_unix_secs(), MAX_REPLICA_DELTA_BYTES)?;
        tx.put(key, value)?;
        if append
            && let Some(id) = access.session_id()
            && self.world_append_scope(&self.session(id)?, address.volume)?
        {
            // Persist the key, value digest and original clock in the backing's
            // private index, atomically with the KV birth. Values stay in place.
            let marker = crate::store::key_value::kv_append_key(tx.clock());
            tx.local_indexes()
                .put(&marker, &crate::store::key_value::kv_append_record(key, value))?;
        }

        let delta = {
            let _commit = crate::trace::Span::new("storage.commit");
            tx.commit_with_delta()?
        };
        self.changes.publish(address.volume, DatabaseId::Entry(address.entry), delta);
        Ok(())
    }

    pub fn kv_delete(&self, access: &Access, address: EntryAddress, key: &[u8]) -> Result<()> {
        let _timing = crate::trace::Span::new("storage.kv_delete");
        key_value::validate_key(key)?;
        let operations = self.access_operations(access, address.volume)?;
        let _guards = operations.lock()?;
        let database = self.kv_backing(access, address, Permission::Write)?;
        if database.begin_read()?.value_len(key)?.is_none() {
            return Ok(());
        }
        let mut tx = database.begin_write_with_delta(now_unix_secs(), MAX_REPLICA_DELTA_BYTES)?;
        tx.del(key)?;
        let delta = {
            let _commit = crate::trace::Span::new("storage.commit");
            tx.commit_with_delta()?
        };
        self.changes.publish(address.volume, DatabaseId::Entry(address.entry), delta);
        Ok(())
    }
}
