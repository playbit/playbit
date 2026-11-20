//! Connected storage worker host: local data authority and bounded file hydration.
use super::data_host::DataHost;
use crate::runtime::store_bridge::{database_address, error};
use crate::runtime::syscall::*;

#[derive(Clone)]
pub struct LocalHost {
    pub data: DataHost,
    pub blobs: crate::engine::blob::BlobTransport,
    pub session: String,
}
impl Host for LocalHost {
    fn supported_operations(&self) -> &[Opcode] {
        &[Opcode::FileReadBegin, Opcode::FileWriteBegin]
    }
    fn bloblog_append(
        &mut self,
        entry: EntryAddress,
        append_id: [u8; 16],
        blobs: &[Vec<u8>],
    ) -> HostResult<crate::store::bloblog::Cursor> {
        self.data.bloblog_append(entry, append_id, blobs)
    }
    fn bloblog_read(
        &mut self,
        entry: EntryAddress,
        cursor: Option<&crate::store::bloblog::Cursor>,
    ) -> HostResult<crate::store::bloblog::Page> {
        self.data.bloblog_read(entry, cursor)
    }
    fn kv_keys(
        &mut self,
        entry: crate::runtime::host::EntryAddress,
        after: Option<&[u8]>,
        root: Option<crate::store::key_value::Root>,
        limit: u32,
    ) -> HostResult<crate::store::key_value::Page> {
        self.data.kv_keys(entry, after, root, limit)
    }
    fn kv_read(
        &mut self,
        entry: crate::runtime::host::EntryAddress,
        key: &[u8],
        offset: u64,
        limit: u32,
        root: Option<crate::store::key_value::Root>,
    ) -> HostResult<crate::store::key_value::Chunk> {
        self.data.kv_read(entry, key, offset, limit, root)
    }
    fn kv_put(
        &mut self,
        entry: crate::runtime::host::EntryAddress,
        key: &[u8],
        value: &[u8],
    ) -> HostResult<()> {
        self.data.kv_put(entry, key, value)
    }
    fn kv_delete(
        &mut self,
        entry: crate::runtime::host::EntryAddress,
        key: &[u8],
    ) -> HostResult<()> {
        self.data.kv_delete(entry, key)
    }
    fn resolve_entry(&mut self, volume: Id, name: &str) -> HostResult<[u8; 15]> {
        self.data.resolve_entry(volume, name)
    }
    fn schema(&mut self, entry: EntryAddress) -> HostResult<Schema> {
        self.data.schema(entry)
    }
    fn establish_schema(&mut self, entry: EntryAddress, schema: &Schema) -> HostResult<()> {
        self.data.establish_schema(entry, schema)
    }
    fn create_object(
        &mut self,
        entry: EntryAddress,
        ty: Id,
        fields: &[FieldChange],
    ) -> HostResult<Id> {
        self.data.create_object(entry, ty, fields)
    }
    fn read_object(
        &mut self,
        entry: EntryAddress,
        object: Id,
        fields: &[Id],
    ) -> HostResult<ObjectRecord> {
        self.data.read_object(entry, object, fields)
    }
    fn edit_fields(
        &mut self,
        entry: EntryAddress,
        object: Id,
        fields: &[FieldChange],
    ) -> HostResult<()> {
        self.data.edit_fields(entry, object, fields)
    }
    fn query_objects(
        &mut self,
        entry: EntryAddress,
        ty: Id,
        after: Option<Id>,
        limit: u32,
        fields: Option<&[Id]>,
    ) -> HostResult<ObjectPage> {
        self.data.query_objects(entry, ty, after, limit, fields)
    }
    fn read_authenticated_field(
        &mut self,
        entry: EntryAddress,
        object: Id,
        field: Id,
    ) -> HostResult<Option<AuthenticatedFieldRead>> {
        self.data.read_authenticated_field(entry, object, field)
    }
    fn file_metadata(&mut self, entry: EntryAddress) -> HostResult<(u64, [u8; 32])> {
        self.data.file_metadata(entry)
    }
    fn read_file(&mut self, entry: EntryAddress, offset: u64, limit: u32) -> HostResult<FileChunk> {
        if limit == 0 || limit as usize > crate::store::BLOB_CHUNK_BYTES {
            return Err(HostError::InvalidArgument);
        }
        let (version, bytes) = self
            .blobs
            .read_range(
                self.data.store.clone(),
                self.data.access.clone(),
                (!self.session.is_empty()).then(|| self.session.clone()),
                database_address(entry)?,
                offset,
                limit,
            )
            .map_err(|failure| {
                failure.downcast::<crate::store::Error>().map_or(HostError::Unavailable, |e| {
                    match *e {
                        crate::store::Error::NotFound("blob content") => HostError::Unavailable,
                        error_value => error(error_value),
                    }
                })
            })?;
        Ok(FileChunk { total_bytes: version.len_bytes, bytes })
    }
    fn write_file(&mut self, entry: EntryAddress, bytes: &[u8]) -> HostResult<()> {
        self.data.write_file(entry, bytes)
    }
}
#[cfg(all(test, feature = "app-host"))]
#[allow(clippy::unwrap_used, reason = "Tests assert actual stable entry addressing.")]
mod tests {
    use super::*;
    use crate::store::{Access, volume_index::EntryKind};
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    #[test]
    fn connected_host_resolves_once_and_uses_the_same_database_after_rename() {
        let directory = tempfile::tempdir().unwrap();
        let engine = crate::engine::Engine::open(directory.path()).unwrap();
        let store = &engine.services.store;
        let volume = store.create_volume("Connected fixture", false).unwrap();
        let access = Access::owner(store.account().id);
        let entry = store
            .create_database(
                &access,
                volume.id,
                "data",
                EntryKind::ObjectDb { schema: Default::default() },
            )
            .unwrap();
        let changed = Arc::new(AtomicBool::new(false));
        let mut host = LocalHost {
            data: DataHost {
                store: store.clone(),
                access: access.clone(),
                changed: changed.clone(),
            },
            blobs: engine.network.blobs.clone(),
            session: String::new(),
        };
        let entry_id = host.resolve_entry(*volume.id.as_bytes(), "data").unwrap();
        assert_eq!(entry_id, *entry.id.as_bytes());
        let address = EntryAddress { volume_id: *volume.id.as_bytes(), entry_id };
        store.rename_entry(&access, volume.id, entry.id, "renamed").unwrap();
        assert!(host.resolve_entry(address.volume_id, "data").is_err());
        assert_eq!(host.schema(address).unwrap(), Schema::default());
        assert!(!changed.load(Ordering::Acquire));
    }
}
