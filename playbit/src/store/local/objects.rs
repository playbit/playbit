//! Object operations resolve one named database through the persistent index.
//! Authorization, schema and backing snapshots share the volume boundary.
use super::*;
use crate::store::{
    EntryId, MAX_REPLICA_DELTA_BYTES, object_database,
    volume_index::{EntryAddress, EntryKind, IndexSnapshot},
};

impl LocalStore {
    pub(super) fn object_database_schema(
        &self,
        access: &Access,
        address: EntryAddress,
        permission: Permission,
        require_backing: bool,
    ) -> Result<Schema> {
        let policy = self.index(address.volume)?;
        self.check_authority(access, address.volume, Some(address.entry))?;
        if self.admit_access(access, &policy)?.is_some_and(|grant| !grant.write)
            && permission != Permission::Read
        {
            return Err(Error::AccessDenied("personal volume is read-only"));
        }
        let index = self.db(address.volume, crate::store::DatabaseId::Index)?;
        let snapshot = IndexSnapshot::new(index.begin_read()?);
        snapshot.authorize(
            policy.volume(),
            policy.default_permissions(),
            self.data_actor(access, policy.volume())?,
            address.entry,
            permission,
        )?;
        let entry = snapshot.live_entry(address.entry)?;
        let EntryKind::ObjectDb { schema } = entry.info.kind else {
            return Err(Error::InvalidInput("entry is not an object database"));
        };
        if require_backing && !snapshot.backing_ready(address.entry)? {
            return Err(Error::NotFound("object database content is pending"));
        }
        Ok(schema)
    }

    pub fn schema(&self, access: &Access, address: EntryAddress) -> Result<Schema> {
        let operations = self.access_operations(access, address.volume)?;
        let _guards = operations.lock()?;
        self.object_database_schema(access, address, Permission::Read, false)
    }

    pub fn establish_schema(
        &self,
        access: &Access,
        address: EntryAddress,
        declaration: &Schema,
    ) -> Result<()> {
        let operations = self.access_operations(access, address.volume)?;
        let _guards = operations.lock()?;
        self.object_database_schema(access, address, Permission::Write, false)?;
        let database = self.db(address.volume, crate::store::DatabaseId::Index)?;
        let mut tx = database.begin_write_with_delta(now_unix_secs(), MAX_REPLICA_DELTA_BYTES)?;
        if !crate::store::volume_index::establish_schema(
            &mut tx,
            address.entry,
            declaration,
            EntryId::generate()?,
            &crate::store::volume_index::namespace::Origin {
                installation: self.installation(),
                label: None,
            },
        )? {
            return Ok(());
        }
        let delta = {
            let _commit = crate::trace::Span::new("storage.commit");
            tx.commit_with_delta()?
        };
        self.changes.publish(address.volume, crate::store::DatabaseId::Index, delta);
        Ok(())
    }

    pub fn create_object(
        &self,
        access: &Access,
        address: EntryAddress,
        type_id: Id,
    ) -> Result<ObjectFields> {
        self.create_object_with_fields(access, address, type_id, &BTreeMap::new())
    }

    pub fn create_object_with_fields(
        &self,
        access: &Access,
        address: EntryAddress,
        type_id: Id,
        fields: &BTreeMap<Id, Option<Vec<Value>>>,
    ) -> Result<ObjectFields> {
        let _timing = crate::trace::Span::new("storage.create");
        let operations = self.access_operations(access, address.volume)?;
        let _guards = operations.lock()?;
        let schema = match self.object_database_schema(access, address, Permission::Write, true) {
            Ok(schema) => schema,
            Err(Error::AccessDenied(_)) => {
                self.object_database_schema(access, address, Permission::Append, true)?
            }
            Err(error) => return Err(error),
        };
        let database = self.entry_backing(address.volume, address.entry, false)?;
        let mut tx = database.begin_write_with_delta(now_unix_secs(), MAX_REPLICA_DELTA_BYTES)?;
        let record = object_database::create(
            &mut tx,
            &schema,
            Id::generate()?,
            type_id,
            access.account,
            fields,
        )?;
        let delta = {
            let _commit = crate::trace::Span::new("storage.commit");
            tx.commit_with_delta()?
        };
        self.changes.publish(address.volume, crate::store::DatabaseId::Entry(address.entry), delta);
        Ok(record)
    }

    pub fn read_object(
        &self,
        access: &Access,
        address: EntryAddress,
        object: Id,
    ) -> Result<ObjectFields> {
        self.read_object_fields(access, address, object, None)
    }

    pub fn read_object_fields(
        &self,
        access: &Access,
        address: EntryAddress,
        object: Id,
        fields: Option<&[Id]>,
    ) -> Result<ObjectFields> {
        let _timing = crate::trace::Span::new("storage.read");
        let operations = self.access_operations(access, address.volume)?;
        let _guards = operations.lock()?;
        self.object_database_schema(access, address, Permission::Read, true)?;
        let database = self.entry_backing(address.volume, address.entry, false)?;
        object_database::Snapshot::new(database.begin_read()?).read(object, fields)
    }

    pub fn query_objects(
        &self,
        access: &Access,
        address: EntryAddress,
        type_id: Id,
        after: Option<Id>,
        limit: usize,
    ) -> Result<ObjectPage> {
        self.query_objects_inner(access, address, type_id, after, limit, None)
    }

    pub fn query_object_fields(
        &self,
        access: &Access,
        address: EntryAddress,
        type_id: Id,
        after: Option<Id>,
        limit: usize,
        fields: &[Id],
    ) -> Result<ObjectPage> {
        self.query_objects_inner(access, address, type_id, after, limit, Some(fields))
    }

    fn query_objects_inner(
        &self,
        access: &Access,
        address: EntryAddress,
        type_id: Id,
        after: Option<Id>,
        limit: usize,
        fields: Option<&[Id]>,
    ) -> Result<ObjectPage> {
        let _timing = crate::trace::Span::new("storage.query");
        crate::store::query::validate_projection(fields.unwrap_or(&[]), limit)?;
        let operations = self.access_operations(access, address.volume)?;
        let _guards = operations.lock()?;
        let schema = self.object_database_schema(access, address, Permission::Read, true)?;
        if !schema.types.contains_key(&type_id) {
            return Err(Error::NotFound("object type"));
        }
        let database = self.entry_backing(address.volume, address.entry, false)?;
        object_database::Snapshot::new(database.begin_read()?).page(type_id, after, limit, fields)
    }

    pub fn edit_fields(
        &self,
        access: &Access,
        address: EntryAddress,
        object: Id,
        fields: &BTreeMap<Id, Option<Vec<Value>>>,
    ) -> Result<ObjectFields> {
        let _timing = crate::trace::Span::new("storage.edit");
        let operations = self.access_operations(access, address.volume)?;
        let _guards = operations.lock()?;
        let schema = self.object_database_schema(access, address, Permission::Write, true)?;
        let database = self.entry_backing(address.volume, address.entry, false)?;
        let record = object_database::Snapshot::new(database.begin_read()?).edit_record(object)?;
        let mut tx = database.begin_write_with_delta(now_unix_secs(), MAX_REPLICA_DELTA_BYTES)?;
        #[cfg(test)]
        {
            let hook = self
                .after_edit_read
                .lock()
                .map_err(|_| Error::Internal("object edit hook lock is poisoned"))?
                .clone();
            if let Some(hook) = hook {
                hook(address.volume);
            }
        }
        let mut result = object_database::edit(&mut tx, &schema, record, access.account, fields)?;
        let delta = {
            let _commit = crate::trace::Span::new("storage.commit");
            tx.commit_with_delta()?
        };
        self.changes.publish(address.volume, crate::store::DatabaseId::Entry(address.entry), delta);
        if matches!(
            self.object_database_schema(access, address, Permission::Read, true),
            Err(Error::AccessDenied(_))
        ) {
            result.fields.retain(|field, _| fields.contains_key(field));
        }
        Ok(result)
    }

    pub fn remove_object(&self, access: &Access, address: EntryAddress, object: Id) -> Result<()> {
        let operations = self.access_operations(access, address.volume)?;
        let _guards = operations.lock()?;
        self.object_database_schema(access, address, Permission::Write, true)?;
        let database = self.entry_backing(address.volume, address.entry, false)?;
        let mut tx = database.begin_write_with_delta(now_unix_secs(), MAX_REPLICA_DELTA_BYTES)?;
        object_database::retire(&mut tx, object)?;
        let delta = {
            let _commit = crate::trace::Span::new("storage.commit");
            tx.commit_with_delta()?
        };
        self.changes.publish(address.volume, crate::store::DatabaseId::Entry(address.entry), delta);
        Ok(())
    }
}
