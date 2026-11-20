//! Principal-bound local data operations, independent of VM and AP transport.
use crate::runtime::store_bridge::*;
use crate::runtime::syscall::*;
use crate::store::{Access, LocalStore};
use std::{
    collections::BTreeMap,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

#[derive(Clone)]
pub struct DataHost {
    pub store: Arc<LocalStore>,
    pub access: Access,
    pub changed: Arc<AtomicBool>,
}
impl Host for DataHost {
    fn supported_operations(&self) -> &[Opcode] {
        &[Opcode::FileWriteBegin]
    }
    fn bloblog_append(
        &mut self,
        entry: crate::runtime::host::EntryAddress,
        _append_id: [u8; 16],
        blobs: &[Vec<u8>],
    ) -> HostResult<crate::store::bloblog::Cursor> {
        let entry = database_address(entry)?;
        self.store
            .bloblog_prepare(&self.access, entry.volume, entry.entry, blobs)
            .map_err(error)?;
        Err(HostError::Unavailable)
    }
    fn bloblog_read(
        &mut self,
        entry: crate::runtime::host::EntryAddress,
        cursor: Option<&crate::store::bloblog::Cursor>,
    ) -> HostResult<crate::store::bloblog::Page> {
        let entry = database_address(entry)?;
        let page = self
            .store
            .bloblog_get(&self.access, entry.volume, entry.entry, cursor)
            .map_err(error)?;
        if page.entries.is_empty() {
            return Err(HostError::Unavailable);
        }
        Ok(page)
    }
    fn kv_keys(
        &mut self,
        entry: crate::runtime::host::EntryAddress,
        after: Option<&[u8]>,
        root: Option<crate::store::key_value::Root>,
        limit: u32,
    ) -> HostResult<crate::store::key_value::Page> {
        self.store
            .kv_keys(&self.access, database_address(entry)?, after, root, limit as usize)
            .map_err(error)
    }
    fn kv_read(
        &mut self,
        entry: crate::runtime::host::EntryAddress,
        key: &[u8],
        offset: u64,
        limit: u32,
        root: Option<crate::store::key_value::Root>,
    ) -> HostResult<crate::store::key_value::Chunk> {
        self.store
            .kv_read(&self.access, database_address(entry)?, key, offset, limit, root)
            .map_err(error)
    }
    fn kv_put(
        &mut self,
        entry: crate::runtime::host::EntryAddress,
        key: &[u8],
        value: &[u8],
    ) -> HostResult<()> {
        self.store.kv_put(&self.access, database_address(entry)?, key, value).map_err(error)?;
        self.changed.store(true, Ordering::Release);
        Ok(())
    }
    fn kv_delete(
        &mut self,
        entry: crate::runtime::host::EntryAddress,
        key: &[u8],
    ) -> HostResult<()> {
        self.store.kv_delete(&self.access, database_address(entry)?, key).map_err(error)?;
        self.changed.store(true, Ordering::Release);
        Ok(())
    }
    fn resolve_entry(&mut self, volume: Id, name: &str) -> HostResult<[u8; 15]> {
        self.store.materialize_automatic_access(&self.access, sid(volume)).map_err(error)?;
        self.store
            .resolve_entry(&self.access, sid(volume), name)
            .map(|entry| *entry.id.as_bytes())
            .map_err(error)
    }
    fn schema(&mut self, volume: crate::runtime::host::EntryAddress) -> HostResult<Schema> {
        self.store.schema(&self.access, database_address(volume)?).map(from_schema).map_err(error)
    }
    fn establish_schema(
        &mut self,
        volume: crate::runtime::host::EntryAddress,
        schema: &Schema,
    ) -> HostResult<()> {
        self.store
            .establish_schema(&self.access, database_address(volume)?, &to_schema(schema))
            .map_err(error)?;
        self.changed.store(true, Ordering::Release);
        Ok(())
    }
    fn create_object(
        &mut self,
        volume: crate::runtime::host::EntryAddress,
        type_id: Id,
        changes: &[FieldChange],
    ) -> HostResult<Id> {
        let mut fields = BTreeMap::new();
        for change in changes {
            let values = change
                .values
                .as_ref()
                .map(|values| values.iter().map(to_value).collect::<HostResult<Vec<_>>>())
                .transpose()?;
            if fields.insert(sid(change.field_id), values).is_some() {
                return Err(HostError::InvalidArgument);
            }
        }
        let object = self
            .store
            .create_object_with_fields(
                &self.access,
                database_address(volume)?,
                sid(type_id),
                &fields,
            )
            .map_err(error)?;
        self.changed.store(true, Ordering::Release);
        Ok(*object.object_id.as_bytes())
    }
    fn read_object(
        &mut self,
        volume: crate::runtime::host::EntryAddress,
        object: Id,
        fields: &[Id],
    ) -> HostResult<ObjectRecord> {
        let value = self
            .store
            .read_object_fields(
                &self.access,
                database_address(volume)?,
                sid(object),
                Some(&fields.iter().copied().map(sid).collect::<Vec<_>>()),
            )
            .map_err(error)?;
        Ok(ObjectRecord {
            id: object,
            type_id: *value.type_id.as_bytes(),
            fields: value
                .fields
                .into_iter()
                .map(|(id, cell)| FieldChange {
                    field_id: *id.as_bytes(),
                    values: cell
                        .values
                        .map(|values| values.into_iter().map(from_owned_value).collect()),
                })
                .collect(),
        })
    }
    fn edit_fields(
        &mut self,
        volume: crate::runtime::host::EntryAddress,
        object: Id,
        changes: &[FieldChange],
    ) -> HostResult<()> {
        let fields = changes
            .iter()
            .map(|c| {
                Ok((
                    sid(c.field_id),
                    c.values
                        .as_ref()
                        .map(|v| v.iter().map(to_value).collect::<HostResult<Vec<_>>>())
                        .transpose()?,
                ))
            })
            .collect::<HostResult<_>>()?;
        self.store
            .edit_fields(&self.access, database_address(volume)?, sid(object), &fields)
            .map_err(error)?;
        self.changed.store(true, Ordering::Release);
        Ok(())
    }
    fn query_objects(
        &mut self,
        volume: crate::runtime::host::EntryAddress,
        type_id: Id,
        after: Option<Id>,
        limit: u32,
        projection: Option<&[Id]>,
    ) -> HostResult<ObjectPage> {
        if projection.is_some_and(|fields| fields.is_empty()) {
            return Err(HostError::InvalidArgument);
        }
        if !(1..=playbit_api::abi::syscall::MAX_PAGE_OBJECTS).contains(&limit)
            || projection
                .is_some_and(|fields| fields.len() > playbit_api::abi::syscall::MAX_FIELDS as usize)
        {
            return Err(HostError::LimitExceeded);
        }
        let fields: Vec<_> = projection.unwrap_or_default().iter().map(|id| sid(*id)).collect();
        let page = self
            .store
            .query_object_fields(
                &self.access,
                database_address(volume)?,
                sid(type_id),
                after.map(sid),
                limit as usize,
                &fields,
            )
            .map_err(error)?;
        Ok(ObjectPage {
            objects: page.objects.iter().map(|o| *o.object_id.as_bytes()).collect(),
            next: page.next.map(|id| *id.as_bytes()),
            records: projection.map(|_| {
                page.objects
                    .into_iter()
                    .map(|object| ObjectRecord {
                        id: *object.object_id.as_bytes(),
                        type_id: *object.type_id.as_bytes(),
                        fields: object
                            .fields
                            .into_iter()
                            .map(|(id, cell)| FieldChange {
                                field_id: *id.as_bytes(),
                                values: cell.values.map(|values| {
                                    values.into_iter().map(from_owned_value).collect()
                                }),
                            })
                            .collect(),
                    })
                    .collect()
            }),
        })
    }
    fn file_metadata(
        &mut self,
        address: crate::runtime::host::EntryAddress,
    ) -> HostResult<(u64, [u8; 32])> {
        let version = self
            .store
            .blob_version(&self.access, database_address(address)?, None)
            .map_err(error)?;
        Ok((version.len_bytes, version.digest))
    }
    fn read_file(
        &mut self,
        address: crate::runtime::host::EntryAddress,
        offset: u64,
        limit: u32,
    ) -> HostResult<FileChunk> {
        let version = self
            .store
            .blob_version(&self.access, database_address(address)?, None)
            .map_err(error)?;
        if limit == 0 || limit as usize > crate::store::BLOB_CHUNK_BYTES {
            return Err(HostError::InvalidArgument);
        }
        let file = self.store.verify_blob_version(&self.access, &version).map_err(error)?;
        let bytes = file.read_range(offset, limit).map_err(error)?;
        if self.store.blob_version(&self.access, version.address, None).map_err(error)? != version {
            return Err(HostError::Unavailable);
        }
        Ok(FileChunk { total_bytes: version.len_bytes, bytes })
    }
    fn write_file(
        &mut self,
        address: crate::runtime::host::EntryAddress,
        bytes: &[u8],
    ) -> HostResult<()> {
        let address = database_address(address)?;
        self.store.write_blob(&self.access, address.volume, address.entry, bytes).map_err(error)?;
        self.changed.store(true, Ordering::Release);
        Ok(())
    }
}

#[cfg(all(test, feature = "app-host"))]
mod tests;
