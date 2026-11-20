//! Object meaning belongs to one named database and its current index schema.
use super::*;
use crate::runtime::linear_memory::{Memory, input};
use crate::store::{
    object_database,
    volume_index::{EntryKind, IndexSnapshot},
};

impl Host {
    pub(super) fn entry_handle(
        &self,
        memory: &mut Memory<'_>,
        pointer: u32,
        kind: crate::runtime::host::DataKind,
    ) -> Result<(Rc<Volume>, EntryId)> {
        let (handle, _) = input::entry_handle(memory, pointer, None)?;
        work::capabilities(|capabilities| {
            capabilities.volume(crate::capability::Handle(handle.volume), kind.rights()).map(|_| ())
        })?;
        Ok((
            self.volume_handle(handle.volume >> 32, handle.volume as u32)?,
            EntryId::from_bytes(handle.entry_id).map_err(map_error)?,
        ))
    }
    pub(super) fn schema(
        &self,
        volume: &Volume,
        entry: EntryId,
        permission: Permission,
        backing: bool,
    ) -> Result<Schema> {
        self.authorize(volume, Some(entry), permission)?;
        let snapshot = IndexSnapshot::new(volume.index.begin_read().map_err(map_error)?);
        let EntryKind::ObjectDb { schema } =
            snapshot.live_entry(entry).map_err(store_error)?.info.kind
        else {
            return Err(Error::InvalidArgument);
        };
        if backing && !snapshot.backing_ready(entry).map_err(map_error)? {
            self.need_entry(volume, entry)?;
            return Err(Error::WouldBlock);
        }
        Ok(schema)
    }
    pub(super) fn create(
        &self,
        volume: &Volume,
        entry: EntryId,
        type_id: Id,
        fields: BTreeMap<Id, Option<Vec<Value>>>,
    ) -> Result<Id> {
        let schema = self.schema(volume, entry, Permission::Write, true).or_else(|error| {
            if error == Error::AccessDenied {
                self.schema(volume, entry, Permission::Append, true)
            } else {
                Err(error)
            }
        })?;
        let database = self.entry_database(volume, entry)?;
        let mut tx = database
            .begin_write_with_delta(unsafe { now() } as u64, crate::store::MAX_REPLICA_DELTA_BYTES)
            .map_err(map_error)?;
        let mut id = [0; 16];
        unsafe {
            random(id.as_mut_ptr(), id.len());
        }
        let id = Id::from_bytes(id);
        object_database::create(&mut tx, &schema, id, type_id, self.account, &fields)
            .map_err(store_error)?;
        let delta = tx.commit_with_delta().map_err(map_error)?;
        self.changes.borrow_mut().push(volume.id, DatabaseId::Entry(entry), delta);
        Ok(id)
    }
    pub(super) fn edit(
        &self,
        volume: &Volume,
        entry: EntryId,
        object: Id,
        fields: BTreeMap<Id, Option<Vec<Value>>>,
    ) -> Result<()> {
        let schema = self.schema(volume, entry, Permission::Write, true)?;
        let database = self.entry_database(volume, entry)?;
        let record = object_database::Snapshot::new(database.begin_read().map_err(map_error)?)
            .read(object, None)
            .map_err(store_error)?;
        let mut tx = database
            .begin_write_with_delta(unsafe { now() } as u64, crate::store::MAX_REPLICA_DELTA_BYTES)
            .map_err(map_error)?;
        object_database::edit(&mut tx, &schema, record, self.account, &fields)
            .map_err(map_error)?;
        let delta = tx.commit_with_delta().map_err(map_error)?;
        self.changes.borrow_mut().push(volume.id, DatabaseId::Entry(entry), delta);
        Ok(())
    }
}
