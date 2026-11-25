//! Declaration-bound independent blob initialization, before guest execution.
use super::blob::FileReference;
use super::*;
use crate::store::volume_index::{
    self, BlobVersion, EntryAddress, EntryInfo, EntryKind, IndexSnapshot,
};

impl Host {
    fn initial_blob_reference(&self, volume: Id, entry: EntryId) -> Result<Option<FileReference>> {
        let volume = self.replica_volume(volume)?;
        if self.account != volume.record.owner {
            return Err(Error::AccessDenied);
        }
        let initializer = self.initializer_for(volume.id).ok_or(Error::AccessDenied)?;
        let seed =
            initializer.blobs.iter().find(|seed| seed.id == entry).ok_or(Error::AccessDenied)?;
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
        let volume = self.replica_volume(reference.volume)?;
        if self.account != volume.record.owner {
            return Err(Error::AccessDenied);
        }
        let initializer = self.initializer_for(volume.id).ok_or(Error::AccessDenied)?;
        let seed = initializer
            .blobs
            .iter()
            .find(|seed| seed.id == reference.entry)
            .ok_or(Error::AccessDenied)?;
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
        self.record_blob_upload(&mut tx, &version)?;
        let delta = tx.commit_with_delta().map_err(map_error)?;
        self.committed_change(volume.id, DatabaseId::Index, delta);
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
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Request {
            Scoped { volume: Id, entry: EntryId },
            Primary(EntryId),
        }
        let (volume, entry) = match serde_json::from_slice(&input[..length]).map_err(map_error)? {
            Request::Scoped { volume, entry } => (volume, entry),
            Request::Primary(entry) => (host.volumes.borrow()[0].id, entry),
        };
        serde_json::to_vec(&host.initial_blob_reference(volume, entry)?).map_err(map_error)
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
