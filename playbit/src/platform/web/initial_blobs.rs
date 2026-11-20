//! Declaration-bound independent blob initialization, before guest execution.
use super::blob::FileReference;
use super::*;
use crate::store::{
    InitialBlob,
    volume_index::{self, BlobVersion, EntryAddress, EntryInfo, EntryKind, IndexSnapshot},
};

impl Host {
    fn initial_blob(&self, entry: EntryId) -> Result<&InitialBlob> {
        if self.account != self.volumes.borrow()[0].record.owner {
            return Err(Error::AccessDenied);
        }
        self.initializer
            .as_ref()
            .and_then(|root| root.blobs.iter().find(|seed| seed.id == entry))
            .ok_or(Error::AccessDenied)
    }

    fn initial_blob_reference(&self, entry: EntryId) -> Result<Option<FileReference>> {
        let seed = self.initial_blob(entry)?;
        let volumes = self.volumes.borrow();
        let volume = &volumes[0];
        match IndexSnapshot::new(volume.index.begin_read().map_err(map_error)?).entry(entry) {
            Ok(_) => return Ok(None),
            Err(crate::store::Error::NotFound(_)) => {}
            Err(error) => return Err(store_error(error)),
        }
        let version = BlobVersion {
            address: EntryAddress { volume: volume.id, entry },
            revision: databases::random_entry()?,
            len_bytes: seed.bytes.len() as u64,
            digest: *blake3::hash(&seed.bytes).as_bytes(),
            encrypted_key: [0; 32],
        };
        Ok(Some(FileReference::from_version(version, 0, 0)))
    }

    fn publish_initial_blob(&self, reference: &FileReference) -> Result<()> {
        let seed = self.initial_blob(reference.entry)?;
        let volumes = self.volumes.borrow();
        let volume = &volumes[0];
        let version = reference.version()?;
        if reference.volume != volume.id
            || reference.offset != 0
            || reference.limit != 0
            || version.len_bytes != seed.bytes.len() as u64
            || version.digest != *blake3::hash(&seed.bytes).as_bytes()
            || version.encrypted_key == [0; 32]
        {
            return Err(Error::AccessDenied);
        }
        blob_cache::protect(self, &version)?;
        let mut tx = volume
            .index
            .begin_write_with_delta(unsafe { now() } as u64, crate::store::MAX_REPLICA_DELTA_BYTES)
            .map_err(map_error)?;
        let published_at_micros = super::blob::publication_time()?;
        volume_index::create_at_path(
            &mut tx,
            seed.id,
            &seed.name,
            &EntryInfo {
                namespace: Default::default(),
                origin: crate::store::volume_index::namespace::Origin {
                    installation: self.installation,
                    label: None,
                },
                initial_permissions: crate::store::object_contract::EntryAcl::Inherit,
                created_revision: version.revision,
                name: seed.name.rsplit('/').next().expect("validated seed path").to_owned(),
                protocol: None,
                kind: EntryKind::Blob {
                    len_bytes: version.len_bytes,
                    digest: version.digest,
                    encrypted_key: version.encrypted_key,
                    published_at_micros,
                },
            },
        )
        .map_err(store_error)?;
        volume_index::mark_backing_ready(&mut tx, seed.id).map_err(store_error)?;
        tx.local_indexes()
            .put(&blob::pending_key(&version), &serde_json::to_vec(&version).map_err(map_error)?)
            .map_err(map_error)?;
        let delta = tx.commit_with_delta().map_err(map_error)?;
        self.changes.borrow_mut().push(volume.id, DatabaseId::Index, delta);
        Ok(())
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn pb_seed_blob_reference(length: usize) -> i32 {
    crate::platform::failure::guard();
    let input = match admit_input(length, 128) {
        Ok(input) => input,
        Err(error) => return error as i32,
    };
    let result = HOST.with(|cell| {
        let host = cell.borrow();
        let host = host.as_ref().ok_or(Error::Unavailable)?;
        let entry: EntryId = serde_json::from_slice(&input[..length]).map_err(map_error)?;
        serde_json::to_vec(&host.initial_blob_reference(entry)?).map_err(map_error)
    });
    result.map_or_else(|error| error as i32, publish_output)
}

#[unsafe(no_mangle)]
pub extern "C" fn pb_seed_blob(length: usize) -> i32 {
    crate::platform::failure::guard();
    let input = match admit_input(length, 1024) {
        Ok(input) => input,
        Err(error) => return error as i32,
    };
    let result = HOST.with(|cell| {
        let host = cell.borrow();
        let host = host.as_ref().ok_or(Error::Unavailable)?;
        let reference: FileReference =
            serde_json::from_slice(&input[..length]).map_err(map_error)?;
        host.publish_initial_blob(&reference)
    });
    result.map_or_else(|error| error as i32, |()| 0)
}
