//! Filesystem-worker objects. The engine owns handles and ordering; this module
//! owns descriptors and private contents and never runs on the guest thread.
use super::{
    authority::Authority,
    captured_list,
    host::{Directory, Reader},
    publication::{HostWriter, Publication, Registry},
    snapshot::SnapshotArea,
    staging::{Area, Recovery},
};
use crate::runtime::host::{
    HostResult,
    files::{
        Action, NOT_PUBLISHED, Output, PUBLICATION_UNKNOWN, PUBLISHED, PUBLISHED_DURABILITY_UNKNOWN,
    },
};
use playbit_api::abi::{files::*, syscall::Error};
use std::{io, sync::Arc};

#[derive(Default)]
pub struct Placement {
    pub snapshots: Option<SnapshotArea>,
    pub staging: Option<Area>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Directory,
    Reader,
    Writer,
    Recovery,
    List,
}
impl Kind {
    pub fn admits(self, action: &Action) -> HostResult<()> {
        action.validate()?;
        let valid = match action {
            Action::Open { .. } | Action::ListOpen => self == Self::Directory,
            Action::Read { .. } => matches!(self, Self::Reader | Self::Recovery),
            Action::Write { .. } => self == Self::Writer,
            Action::Status => self != Self::List,
            Action::Close { flags } => {
                self != Self::List && valid_close_flags(*flags, self == Self::Writer)
            }
            Action::ListNext { .. } => self == Self::List,
        };
        if valid { Ok(()) } else { Err(Error::InvalidArgument) }
    }
}
pub enum Object {
    Bundle(crate::package::filesystem::Object),
    Directory { directory: Directory, placement: Arc<Placement> },
    Reader(Reader),
    Writer(HostWriter),
    Recovery(Box<Recovery>),
    List(Arc<captured_list::List>),
    VolumeDirectory(super::volume::Directory),
    VolumeReader(super::volume::Reader),
    VolumeWriter(super::volume::Writer),
    VolumeRecovery(super::volume::Recovery),
    VolumeList(super::volume::List),
}
#[derive(Default)]
pub struct Services {
    pub publication: Registry,
    pub lists: captured_list::Budget,
}
/// A successful open or close-recovery is placed in a pre-reserved engine slot.
/// Placeholder handles are replaced by that slot's token before guest delivery.
pub struct Completion {
    pub output: Output,
    pub created: Option<Object>,
}
impl Completion {
    pub(super) fn output(output: Output) -> Self {
        Self { output, created: None }
    }
    pub(super) fn created(output: Output, created: Object) -> Self {
        Self { output, created: Some(created) }
    }
}
impl Object {
    pub fn kind(&self) -> Kind {
        match self {
            Self::Bundle(crate::package::filesystem::Object::Directory { .. }) => Kind::Directory,
            Self::Bundle(crate::package::filesystem::Object::Reader { .. }) => Kind::Reader,
            Self::Bundle(crate::package::filesystem::Object::List { .. }) => Kind::List,
            Self::Directory { .. } | Self::VolumeDirectory(_) => Kind::Directory,
            Self::Reader(_) | Self::VolumeReader(_) => Kind::Reader,
            Self::Writer(_) | Self::VolumeWriter(_) => Kind::Writer,
            Self::Recovery(_) | Self::VolumeRecovery(_) => Kind::Recovery,
            Self::List(_) | Self::VolumeList(_) => Kind::List,
        }
    }
    pub fn execute(
        &mut self,
        action: Action,
        services: &Services,
        authority: &Arc<Authority>,
    ) -> HostResult<Completion> {
        self.kind().admits(&action)?;
        self.check_directory()?;
        authority.check(matches!(action, Action::Write { .. })).map_err(error)?;
        match (self, action) {
            (Self::Bundle(object), action) => {
                let completion = object.execute(action)?;
                Ok(Completion {
                    output: completion.output,
                    created: completion.created.map(Self::Bundle),
                })
            }
            (Self::VolumeDirectory(directory), Action::Open { path, mode }) => {
                Ok(Completion::created(Output::File { handle: 0 }, directory.open(&path, mode)?))
            }
            (Self::VolumeDirectory(directory), Action::ListOpen) => Ok(Completion::created(
                Output::List { handle: 0 },
                Self::VolumeList(directory.list(&services.lists)?),
            )),
            (Self::VolumeReader(reader), Action::Read { offset, length }) => {
                Ok(Completion::output(Output::Read { bytes: reader.read(offset, length)? }))
            }
            (Self::VolumeRecovery(reader), Action::Read { offset, length }) => {
                Ok(Completion::output(Output::Read { bytes: reader.read(offset, length)? }))
            }
            (Self::VolumeWriter(writer), Action::Write { offset, bytes }) => {
                writer.write(offset, &bytes)?;
                Ok(Completion::output(Output::Written))
            }
            (Self::VolumeList(list), Action::ListNext { index }) => {
                Ok(Completion::output(list_output(list.next_at(index)?)))
            }
            (Self::Directory { directory, placement }, Action::Open { path, mode }) => {
                let created = open_host(directory, placement, &path, mode, services)?;
                Ok(Completion::created(Output::File { handle: 0 }, created))
            }
            (Self::Directory { directory, .. }, Action::ListOpen) => {
                let capture = services.lists.reserve().map_err(list_error)?;
                let entries = directory.list().map_err(error)?;
                let list = capture.finish(entries, authority.clone()).map_err(list_error)?;
                Ok(Completion::created(Output::List { handle: 0 }, Self::List(list)))
            }
            (Self::Reader(reader), Action::Read { offset, length }) => {
                Ok(Completion::output(Output::Read {
                    bytes: reader.read(offset, length as usize).map_err(error)?,
                }))
            }
            (Self::Recovery(reader), Action::Read { offset, length }) => {
                Ok(Completion::output(Output::Read {
                    bytes: reader.read(offset, length as usize).map_err(error)?,
                }))
            }
            (Self::Writer(writer), Action::Write { offset, bytes }) => {
                writer.write(offset, &bytes).map_err(error)?;
                Ok(Completion::output(Output::Written))
            }
            (object, Action::Status) => {
                let status = object.status()?;
                Ok(Completion::output(status.into()))
            }
            (Self::List(list), Action::ListNext { index }) => {
                let output = list_output(list.next_at(index).map_err(list_error)?);
                Ok(Completion::output(output))
            }
            _ => Err(Error::InvalidArgument),
        }
    }
    fn check_directory(&self) -> HostResult<()> {
        if let Self::Directory { directory, .. } = self {
            directory.check().map_err(|failure| {
                if failure.raw_os_error() == Some(libc::ENOENT) {
                    Error::DestinationDeleted
                } else {
                    error(failure)
                }
            })?;
        }
        Ok(())
    }
    fn status(&self) -> HostResult<FileStatus> {
        match self {
            Self::Bundle(object) => object.status(),
            Self::Directory { directory, .. } => directory.status().map_err(error),
            Self::Reader(reader) => reader.status().map_err(error),
            Self::Writer(writer) => writer.status().map_err(error),
            Self::Recovery(reader) => reader.status().map_err(error),
            Self::VolumeDirectory(directory) => directory.status(),
            Self::VolumeReader(reader) => reader.status(),
            Self::VolumeWriter(writer) => writer.status(),
            Self::VolumeRecovery(reader) => reader.status(),
            Self::List(_) | Self::VolumeList(_) => Err(Error::InvalidArgument),
        }
    }
    /// Called only after the owner seals admission and all earlier accepted
    /// operations finish. Closing never revokes the grant shared by descendants.
    pub fn close(self, flags: u32) -> HostResult<Completion> {
        self.kind().admits(&Action::Close { flags })?;
        if flags == FILE_CLOSE_DISCARD || !matches!(self, Self::Writer(_) | Self::VolumeWriter(_)) {
            if let Self::VolumeReader(reader) = self {
                reader.close()?;
            } else {
                drop(self);
            }
            return Ok(Completion::output(Output::Close {
                error: 0,
                publication: NOT_PUBLISHED,
                recovery: None,
            }));
        }
        if let Self::VolumeWriter(writer) = self {
            return Ok(writer.close(flags == FILE_CLOSE_SYNC));
        }
        let Self::Writer(writer) = self else { unreachable!("writer checked before consumption") };
        Ok(match writer.close(flags == FILE_CLOSE_SYNC) {
            Ok(()) => Completion::output(Output::Close {
                error: 0,
                publication: PUBLISHED,
                recovery: None,
            }),
            Err(failure) => {
                let publication = match failure.publication {
                    Publication::NotPublished => NOT_PUBLISHED,
                    Publication::PublishedDurabilityUnknown => PUBLISHED_DURABILITY_UNKNOWN,
                    Publication::Unknown => PUBLICATION_UNKNOWN,
                };
                let created = failure.recovery.map(Self::Recovery);
                Completion {
                    output: Output::Close {
                        error: error(failure.reason) as i32,
                        publication,
                        recovery: created.as_ref().map(|_| 0),
                    },
                    created,
                }
            }
        })
    }
}
fn list_output(entry: Option<super::listing::Entry>) -> Output {
    match entry {
        None => Output::End,
        Some(entry) => Output::Entry {
            kind: match entry.kind {
                super::listing::Kind::File => FILE_KIND_FILE,
                super::listing::Kind::Directory => FILE_KIND_DIRECTORY,
                super::listing::Kind::Symlink => FILE_KIND_SYMLINK,
                super::listing::Kind::Other => FILE_KIND_OTHER,
            },
            name: entry.name,
        },
    }
}
pub fn error(error: io::Error) -> Error {
    match error.raw_os_error() {
        Some(libc::EACCES | libc::EPERM) => Error::AccessDenied,
        Some(libc::ENOENT) => Error::NotFound,
        Some(libc::EEXIST | libc::ESTALE) => Error::Conflict,
        Some(libc::EINVAL | libc::ENOTDIR | libc::ENAMETOOLONG) => Error::InvalidArgument,
        Some(libc::EFBIG | libc::ENOSPC | libc::EDQUOT | libc::E2BIG) => Error::LimitExceeded,
        Some(libc::ENOTSUP | libc::EXDEV) => Error::Unsupported,
        _ => Error::Unavailable,
    }
}
pub(super) fn list_error(error: captured_list::Error) -> Error {
    match error {
        captured_list::Error::Closed => Error::Closed,
        captured_list::Error::Revoked => Error::AccessDenied,
        captured_list::Error::LimitExceeded => Error::LimitExceeded,
        captured_list::Error::InvalidIndex => Error::InvalidArgument,
        captured_list::Error::Internal => Error::Internal,
    }
}

fn open_host(
    directory: &Directory,
    placement: &Arc<Placement>,
    path: &[u8],
    mode: u32,
    services: &Services,
) -> HostResult<Object> {
    let created = match mode {
        FILE_DIRECTORY => Object::Directory {
            directory: directory.directory(path).map_err(error)?,
            placement: placement.clone(),
        },
        FILE_READ => {
            let mut reader = directory.reader(path).map_err(error)?;
            if let Some(area) = &placement.snapshots {
                reader = reader.snapshot(area).map_err(error)?;
            }
            Object::Reader(reader)
        }
        mode if mode == FILE_WRITE || mode == FILE_WRITE | FILE_EXCLUSIVE => Object::Writer(
            HostWriter::open(
                directory,
                path,
                mode & FILE_EXCLUSIVE != 0,
                placement.staging.as_ref(),
                &services.publication,
            )
            .map_err(error)?,
        ),
        _ => return Err(Error::InvalidArgument),
    };
    Ok(created)
}
