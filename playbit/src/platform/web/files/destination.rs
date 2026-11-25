//! A writer binds identity and attachment at Open. Publication rechecks that
//! binding after private content preparation; another incarnation never receives
//! these bytes merely because it acquired the same path.
use super::objects::Directory;
use super::*;
use crate::store::{
    object_contract::EntryAcl,
    volume_index::{self, EntryInfo, EntryKind, IndexSnapshot},
};

pub(super) struct Destination {
    directory: Directory,
    parent: Option<EntryId>,
    pub(super) entry: EntryId,
    name: String,
    create: bool,
    exclusive: bool,
    attachment: Option<(EntryId, EntryId)>,
    context: Option<volume_index::namespace::CreationContext>,
}
fn create_allowed(host: &Host, volume: &Volume) -> Result<()> {
    match host.authorize(volume, None, Permission::Write) {
        Err(Error::AccessDenied) => {
            host.authorize(volume, None, Permission::Append)?;
        }
        Err(error) => return Err(error),
        Ok(_) => {}
    }
    Ok(())
}
fn origin(host: &Host) -> volume_index::namespace::Origin {
    volume_index::namespace::Origin { installation: host.installation, label: None }
}
fn parents(host: &Host, directory: &Directory, path: &str) -> Result<(Option<EntryId>, String)> {
    let Some((path, name)) = path.rsplit_once('/') else {
        return Ok((directory.parent, path.into()));
    };
    let volume = directory.check(host, false)?;
    let snapshot = IndexSnapshot::new(volume.index.begin_read().map_err(map_error)?);
    let parent = match snapshot.lookup_from(directory.parent, path) {
        Ok(entry) if matches!(entry.info.kind, EntryKind::Directory {}) => entry.id,
        Ok(_) => return Err(Error::InvalidArgument),
        Err(crate::store::Error::NotFound(_)) => {
            create_allowed(host, &volume)?;
            let parent = databases::random_entry()?;
            let info = EntryInfo {
                namespace: volume_index::namespace::Birth {
                    parent: directory.parent,
                    automatic: true,
                    ..Default::default()
                },
                origin: origin(host),
                initial_permissions: EntryAcl::Inherit,
                created_revision: databases::random_entry()?,
                name: path.rsplit('/').next().expect("validated path").into(),
                kind: EntryKind::Directory {},
                protocol: None,
            };
            let mut tx = volume
                .index
                .begin_write_with_delta(
                    unsafe { now() } as u64,
                    crate::store::MAX_REPLICA_DELTA_BYTES,
                )
                .map_err(map_error)?;
            volume_index::create_at_path(&mut tx, parent, path, &info).map_err(store_error)?;
            let delta = tx.commit_with_delta().map_err(map_error)?;
            host.committed_change(volume.id, DatabaseId::Index, delta);
            parent
        }
        Err(error) => return Err(store_error(error)),
    };
    Ok((Some(parent), name.into()))
}
impl Destination {
    pub(super) fn open(
        host: &Host,
        directory: &Directory,
        path: &[u8],
        exclusive: bool,
    ) -> Result<Self> {
        let path = std::str::from_utf8(path).map_err(|_| Error::InvalidArgument)?;
        volume_index::validate_name(path).map_err(store_error)?;
        let volume = directory.check(host, false)?;
        host.admit_volume(&volume, Permission::Write)?;
        let (parent, name) = parents(host, directory, path)?;
        Directory { volume: volume.id, parent }.check(host, false)?;
        let policy = host.admit_volume(&volume, Permission::Write)?;
        let snapshot = IndexSnapshot::new(volume.index.begin_read().map_err(map_error)?);
        let (entry, create, attachment) = match snapshot.lookup_from(parent, &name) {
            Ok(_) if exclusive => return Err(Error::Conflict),
            Ok(entry) => {
                if !matches!(entry.info.kind, EntryKind::Blob { .. }) {
                    return Err(Error::InvalidArgument);
                }
                authority::check(host, &volume, Some(entry.id))?;
                snapshot
                    .authorize_namespace(
                        policy.volume(),
                        policy.default_permissions(),
                        host.actor(&volume),
                        entry.id,
                        Permission::Write,
                    )
                    .map_err(store_error)?;
                let location = snapshot.namespace_entry(entry.id).map_err(store_error)?;
                (entry.id, false, Some((location.parent_event, location.name_event)))
            }
            Err(crate::store::Error::NotFound(_)) => {
                create_allowed(host, &volume)?;
                (databases::random_entry()?, true, None)
            }
            Err(error) => return Err(store_error(error)),
        };
        let context = if create {
            Some(
                volume_index::namespace::observations::Publication::capture(
                    &volume.index.begin_read().map_err(map_error)?,
                )
                .map_err(store_error)?,
            )
        } else {
            None
        };
        Ok(Self {
            directory: directory.clone(),
            parent,
            entry,
            name,
            create,
            exclusive,
            attachment,
            context,
        })
    }
    /// Private staged contents survive namespace deletion, but not revoked
    /// participant, directory Read, original entry Write or installation grant.
    pub(super) fn staging_access(&self, host: &Host) -> Result<Rc<Volume>> {
        let volume = self.directory.check(host, true)?;
        Directory { volume: volume.id, parent: self.parent }.check(host, true)?;
        let policy = host.admit_volume(&volume, Permission::Write)?;
        if self.create {
            create_allowed(host, &volume)?;
        } else {
            authority::check(host, &volume, Some(self.entry))?;
            IndexSnapshot::new(volume.index.begin_read().map_err(map_error)?)
                .authorize_staged_write(
                    policy.volume(),
                    policy.default_permissions(),
                    host.actor(&volume),
                    self.entry,
                )
                .map_err(store_error)?;
        }
        Ok(volume)
    }
    fn validate(&self, host: &Host) -> Result<Rc<Volume>> {
        let volume = self.staging_access(host)?;
        let policy = host.admit_volume(&volume, Permission::Write)?;
        let snapshot = IndexSnapshot::new(volume.index.begin_read().map_err(map_error)?);
        snapshot.directory(self.parent).map_err(|error| match error {
            crate::store::Error::NotFound(_) => Error::DestinationDeleted,
            error => store_error(error),
        })?;
        if self.create && self.exclusive {
            match snapshot.lookup_from(self.parent, &self.name) {
                Ok(_) => return Err(Error::Conflict),
                Err(crate::store::Error::NotFound(_)) => {}
                Err(error) => return Err(store_error(error)),
            }
        } else if !self.create {
            snapshot
                .authorize_namespace(
                    policy.volume(),
                    policy.default_permissions(),
                    host.actor(&volume),
                    self.entry,
                    Permission::Write,
                )
                .map_err(store_error)?;
            let entry = snapshot.namespace_entry(self.entry).map_err(store_error)?;
            if entry.parent != snapshot.canonical_directory(self.parent).map_err(store_error)?
                || Some((entry.parent_event, entry.name_event)) != self.attachment
            {
                return Err(Error::Conflict);
            }
        }
        drop(snapshot);
        Ok(volume)
    }
    pub(super) fn reference(
        &self,
        host: &Host,
        length: u64,
        digest: [u8; 32],
    ) -> Result<blob::FileReference> {
        self.validate(host)?;
        Ok(blob::FileReference::from_version(
            volume_index::BlobVersion {
                address: volume_index::EntryAddress {
                    volume: self.directory.volume,
                    entry: self.entry,
                },
                revision: databases::random_entry()?,
                len_bytes: length,
                digest,
                encrypted_key: [0; 32],
            },
            0,
            0,
        ))
    }
    /// Caller has already flushed complete encrypted/plain contents. The boolean
    /// distinguishes pre-publication failures from uncertain transaction commits.
    pub(super) fn publish(
        &self,
        host: &Host,
        reference: &blob::FileReference,
    ) -> std::result::Result<(), (Error, bool)> {
        let mut uncertain = false;
        let result = (|| {
            let volume = self.validate(host)?;
            let version = reference.version()?;
            if version.address
                != (volume_index::EntryAddress { volume: volume.id, entry: self.entry })
                || reference.offset != 0
                || reference.limit != 0
                || version.encrypted_key == [0; 32]
            {
                return Err(Error::InvalidArgument);
            }
            let kind = EntryKind::Blob {
                len_bytes: version.len_bytes,
                digest: version.digest,
                encrypted_key: version.encrypted_key,
                published_at_micros: blob::publication_time()?,
            };
            blob_cache::protect(host, &version)?;
            let mut tx = volume
                .index
                .begin_write_with_delta(
                    unsafe { now() } as u64,
                    crate::store::MAX_REPLICA_DELTA_BYTES,
                )
                .map_err(map_error)?;
            if self.create {
                let info = self.birth_info(host, version.revision, kind.clone());
                if self.exclusive {
                    volume_index::create(&mut tx, self.entry, &info)
                } else {
                    volume_index::create_competing_blob(&mut tx, self.entry, &info)
                }
                .map_err(store_error)?;
            } else {
                volume_index::update_file_content(
                    &mut tx,
                    self.entry,
                    kind.clone(),
                    version.revision,
                    &origin(host),
                    None,
                )
                .map_err(store_error)?;
            }
            volume_index::mark_published_blob_ready(&mut tx, self.entry, &kind)
                .map_err(store_error)?;
            host.record_blob_upload(&mut tx, &version)?;
            uncertain = true;
            let delta = tx.commit_with_delta().map_err(map_error)?;
            host.committed_change(volume.id, DatabaseId::Index, delta);
            Ok(())
        })();
        result.map_err(|error| (error, uncertain))
    }
}

impl Destination {
    fn birth_info(&self, host: &Host, revision: EntryId, kind: EntryKind) -> EntryInfo {
        EntryInfo {
            namespace: volume_index::namespace::Birth {
                parent: self.parent,
                // Supply the captured observations for pre-insertion validation.
                // Insertion replaces them with the current publication context.
                observed: self
                    .context
                    .as_ref()
                    .map_or_else(Vec::new, |context| context.observed.clone()),
                creation_context: self.context.clone(),
                ..Default::default()
            },
            origin: origin(host),
            initial_permissions: EntryAcl::Inherit,
            created_revision: revision,
            name: self.name.clone(),
            kind,
            protocol: None,
        }
    }
}
