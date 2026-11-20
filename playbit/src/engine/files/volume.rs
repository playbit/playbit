//! Worker-owned volume Files. Directories retain identities; readers retain exact
//! revisions; writers publish complete private bytes under current store authority.
use super::{authority::Authority, captured_list, listing, resource, staging};
use crate::{
    engine::blob::{BlobTransport, ReadView},
    runtime::host::{
        HostResult,
        files::{NOT_PUBLISHED, Output, PUBLICATION_UNKNOWN, PUBLISHED},
    },
    store::{
        Access, EntryId, EntryType, FileStagingAccess, Id, LocalStore,
        volume_index::{BlobVersion, EntryAddress, EntryKind},
    },
};
use playbit_api::abi::{files::*, syscall::Error};
use std::{io::Seek, sync::Arc};

/// Store policy changes invalidate prepared delivery without putting storage
/// work on the engine owner. Capture before the final worker authorization.
pub(super) struct Fence {
    store: Arc<LocalStore>,
    volumes: Vec<Id>,
    revision: Option<(Id, u64)>,
}
impl Fence {
    fn capture(store: Arc<LocalStore>, volume: Id, access: &Access) -> HostResult<Self> {
        let volumes = store.file_delivery_volumes(access, volume).map_err(error)?;
        let revision = store.delivery_scope_revision(None, &volumes).0;
        Ok(Self { store, volumes, revision })
    }
    pub fn current(&self) -> bool {
        matches!(self.store.delivery_scope_revision(self.revision, &self.volumes), (Some(_), None))
            && self.revision.is_some()
    }
}
#[derive(Clone)]
pub struct Directory {
    pub store: Arc<LocalStore>,
    pub access: Access,
    pub volume: Id,
    pub parent: Option<EntryId>,
    pub session: Option<String>,
    pub transport: BlobTransport,
    pub placement: super::executor::PlacementSource,
    pub authority: Arc<Authority>,
}
impl Directory {
    pub fn check(&self) -> HostResult<()> {
        let _timing = crate::trace::Span::new("file.volume.directory_check");
        self.authority.check(false).map_err(resource::error)?;
        self.store.materialize_automatic_access(&self.access, self.volume).map_err(error)?;
        self.store.file_directory(&self.access, self.volume, self.parent).map_err(error)
    }
    fn retained_access(&self) -> HostResult<()> {
        self.authority.check(false).map_err(resource::error)?;
        self.store
            .file_retained_directory_access(&self.access, self.volume, self.parent)
            .map_err(error)
    }
    pub fn open(&self, path: &[u8], mode: u32) -> HostResult<resource::Object> {
        let _timing = crate::trace::Span::new("file.volume.open");
        let path = std::str::from_utf8(path).map_err(|_| Error::InvalidArgument)?;
        // Both store resolutions below validate this retained directory under
        // the access locks together with the selected child's authority.
        self.authority.check(false).map_err(resource::error)?;
        self.store.materialize_automatic_access(&self.access, self.volume).map_err(error)?;
        if mode == FILE_WRITE || mode == FILE_WRITE | FILE_EXCLUSIVE {
            return self.writer(path, mode);
        }
        let entry =
            self.store.file_entry(&self.access, self.volume, self.parent, path).map_err(error)?;
        match (mode, entry.info.kind) {
            (FILE_DIRECTORY, EntryKind::Directory {}) => {
                Ok(resource::Object::VolumeDirectory(Self {
                    parent: Some(entry.id),
                    ..self.clone()
                }))
            }
            (
                FILE_READ,
                EntryKind::Blob { len_bytes, digest, encrypted_key, published_at_micros },
            ) => {
                let version = BlobVersion {
                    address: EntryAddress { volume: self.volume, entry: entry.id },
                    revision: entry.content_revision,
                    len_bytes,
                    digest,
                    encrypted_key,
                };
                let view = self
                    .transport
                    .open_read_version(
                        self.store.clone(),
                        self.access.clone(),
                        self.session.clone(),
                        version,
                    )
                    .map_err(transport_error)?;
                Ok(resource::Object::VolumeReader(Reader {
                    access: self.access.clone(),
                    view,
                    store: self.store.clone(),
                    volume: self.volume,
                    transport: self.transport.clone(),
                    status: FileStatus {
                        kind: FILE_KIND_FILE,
                        flags: FILE_SIZE_VALID
                            | FILE_MODIFICATION_TIME_VALID
                            | FILE_CONTENTS_IMMUTABLE,
                        file_size: len_bytes,
                        modification_time: published_at_micros,
                        ..FileStatus::default()
                    },
                }))
            }
            _ => Err(Error::InvalidArgument),
        }
    }
    fn writer(&self, path: &str, mode: u32) -> HostResult<resource::Object> {
        self.authority.check(true).map_err(resource::error)?;
        let placement = self.placement.get().map_err(resource::error)?;
        let area = placement.staging.as_ref().ok_or(Error::Unavailable)?;
        let stage = {
            let _timing = crate::trace::Span::new("file.volume.stage_create");
            area.create_unlinked(self.authority.clone()).map_err(resource::error)?
        };
        let target = {
            let _timing = crate::trace::Span::new("file.volume.destination");
            self.store
                .file_destination(
                    &self.access,
                    self.volume,
                    self.parent,
                    path,
                    mode & FILE_EXCLUSIVE != 0,
                )
                .map_err(error)?
        };
        let staging_access =
            self.store.capture_file_staging(&self.access, target).map_err(error)?;
        Ok(resource::Object::VolumeWriter(Writer {
            stage,
            staging_access,
            directory: self.clone(),
        }))
    }
    pub fn list(&self, budget: &captured_list::Budget) -> HostResult<List> {
        self.check()?;
        let capture = budget.reserve().map_err(resource::list_error)?;
        let entries =
            self.store.file_entries(&self.access, self.volume, self.parent).map_err(error)?;
        let entries = entries
            .into_iter()
            .map(|entry| listing::Entry {
                name: entry.name.into_bytes(),
                kind: match entry.kind {
                    EntryType::Blob => listing::Kind::File,
                    EntryType::Directory => listing::Kind::Directory,
                    _ => listing::Kind::Other,
                },
            })
            .collect();
        let list = capture.finish(entries, self.authority.clone()).map_err(resource::list_error)?;
        self.retained_access()?;
        Ok(List { list, directory: self.clone() })
    }
    pub fn status(&self) -> HostResult<FileStatus> {
        self.check()?;
        Ok(FileStatus { kind: FILE_KIND_DIRECTORY, ..FileStatus::default() })
    }
}

pub struct Reader {
    view: ReadView,
    access: Access,
    store: Arc<LocalStore>,
    volume: Id,
    transport: BlobTransport,
    status: FileStatus,
}
impl Reader {
    pub fn close(self) -> HostResult<()> {
        self.view.close().map_err(transport_error)
    }
    pub fn read(&mut self, offset: u64, length: u32) -> HostResult<Vec<u8>> {
        self.view.read(&self.transport, offset, length).map_err(transport_error)
    }
    pub fn status(&self) -> HostResult<FileStatus> {
        self.view.version().map_err(transport_error)?;
        Ok(self.status)
    }
}

pub struct Writer {
    stage: staging::Writer,
    staging_access: FileStagingAccess,
    directory: Directory,
}
impl Writer {
    pub fn write(&mut self, offset: u64, bytes: &[u8]) -> HostResult<()> {
        let result = self
            .directory
            .store
            .check_file_staging(&self.staging_access)
            .map_err(error)
            .and_then(|()| self.stage.write(offset, bytes).map_err(resource::error));
        if result.is_err() {
            self.stage.fail();
        }
        result
    }
    pub fn status(&self) -> HostResult<FileStatus> {
        self.directory.store.check_file_staging(&self.staging_access).map_err(error)?;
        Ok(FileStatus::private(self.stage.length().map_err(resource::error)?, false))
    }
    pub fn close(self, sync: bool) -> resource::Completion {
        let _timing = crate::trace::Span::new("file.volume.publish");
        let Self { stage, staging_access, directory } = self;
        let sealed = match stage.seal() {
            Ok(sealed) => sealed,
            Err(failure) => return close_failure(resource::error(failure), NOT_PUBLISHED, None),
        };
        let result = (|| {
            directory.authority.admit_publication().map_err(resource::error)?;
            let mut contents = &*sealed.storage.file;
            contents.rewind().map_err(resource::error)?;
            Ok(directory.store.publish_staged_file(&staging_access, &mut contents, sync))
        })();
        let (reason, publication) = match result {
            Ok(Ok(())) => {
                return resource::Completion::output(Output::Close {
                    error: 0,
                    publication: PUBLISHED,
                    recovery: None,
                });
            }
            Ok(Err(failure)) => (
                error(failure.error),
                if failure.publication_unknown { PUBLICATION_UNKNOWN } else { NOT_PUBLISHED },
            ),
            Err(reason) => (reason, NOT_PUBLISHED),
        };
        let recovery = if publication == NOT_PUBLISHED
            && directory.store.check_file_staging(&staging_access).is_ok()
        {
            sealed.recovery().ok().map(|contents| {
                resource::Object::VolumeRecovery(Recovery { contents, directory, staging_access })
            })
        } else {
            None
        };
        close_failure(reason, publication, recovery)
    }
}
fn close_failure(
    reason: Error,
    publication: u32,
    recovery: Option<resource::Object>,
) -> resource::Completion {
    resource::Completion {
        output: Output::Close {
            error: reason as i32,
            publication,
            recovery: recovery.as_ref().map(|_| 0),
        },
        created: recovery,
    }
}

pub struct Recovery {
    contents: staging::Recovery,
    directory: Directory,
    staging_access: FileStagingAccess,
}
impl Recovery {
    pub fn read(&self, offset: u64, length: u32) -> HostResult<Vec<u8>> {
        self.directory.store.check_file_staging(&self.staging_access).map_err(error)?;
        self.contents.read(offset, length as usize).map_err(resource::error)
    }
    pub fn status(&self) -> HostResult<FileStatus> {
        self.directory.store.check_file_staging(&self.staging_access).map_err(error)?;
        self.contents.status().map_err(resource::error)
    }
}
pub struct List {
    list: Arc<captured_list::List>,
    directory: Directory,
}
impl List {
    pub fn next_at(&self, index: u32) -> HostResult<Option<listing::Entry>> {
        self.directory.retained_access()?;
        self.list.next_at(index).map_err(resource::list_error)
    }
}
fn error(error: crate::store::Error) -> Error {
    match error {
        crate::store::Error::NotFound("destination directory") => Error::DestinationDeleted,
        error => crate::runtime::store_bridge::error(error),
    }
}
fn transport_error(error: Box<dyn std::error::Error + Send + Sync>) -> Error {
    match error.downcast::<crate::store::Error>() {
        Ok(error) => match *error {
            crate::store::Error::NotFound("blob content") => Error::Unavailable,
            error => self::error(error),
        },
        Err(_) => Error::Unavailable,
    }
}

impl resource::Object {
    pub(super) fn capture_delivery_fence(&self) -> HostResult<Option<Fence>> {
        let (store, volume, access) = match self {
            Self::VolumeDirectory(directory) => {
                (&directory.store, directory.volume, &directory.access)
            }
            Self::VolumeReader(reader) => (&reader.store, reader.volume, &reader.access),
            Self::VolumeWriter(writer) => {
                (&writer.directory.store, writer.directory.volume, &writer.directory.access)
            }
            Self::VolumeRecovery(recovery) => {
                (&recovery.directory.store, recovery.directory.volume, &recovery.directory.access)
            }
            Self::VolumeList(list) => {
                (&list.directory.store, list.directory.volume, &list.directory.access)
            }
            _ => return Ok(None),
        };
        Fence::capture(store.clone(), volume, access).map(Some)
    }

    pub(super) fn delivery_fence(&self) -> HostResult<Option<Fence>> {
        let _timing = crate::trace::Span::new("file.volume.delivery");
        let Some(fence) = self.capture_delivery_fence()? else { return Ok(None) };
        match self {
            Self::VolumeDirectory(directory) => {
                directory.check()?;
            }
            Self::VolumeReader(reader) => {
                reader.status()?;
            }
            Self::VolumeWriter(writer) => {
                writer.status()?;
            }
            Self::VolumeRecovery(recovery) => {
                recovery.status()?;
            }
            Self::VolumeList(list) => {
                list.directory.retained_access()?;
            }
            _ => unreachable!("volume object selected above"),
        }
        Ok(Some(fence))
    }
}
