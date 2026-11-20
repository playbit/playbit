//! Read-only File objects over one verified package. The host schedules execute
//! on its existing File worker; objects retain their backing through retirement.
use super::Package;
use crate::runtime::host::{
    HostResult,
    files::{Action, NOT_PUBLISHED, Output},
};
use playbit_api::abi::{files::*, syscall::Error};
use std::sync::Arc;

pub enum Object {
    Directory { package: Package, path: String },
    Reader { package: Package, path: String },
    List { package: Package, path: String, entries: Arc<[usize]> },
}
pub struct Completion {
    pub output: Output,
    pub created: Option<Object>,
}
impl Object {
    pub fn root(package: Package) -> Self {
        Self::Directory { package, path: String::new() }
    }
    pub fn status(&self) -> HostResult<FileStatus> {
        match self {
            Self::Directory { .. } => {
                Ok(FileStatus { kind: FILE_KIND_DIRECTORY, ..FileStatus::default() })
            }
            Self::Reader { package, path } => Ok(FileStatus::private(
                package.index().entry(path).ok_or(Error::Internal)?.length,
                true,
            )),
            Self::List { .. } => Err(Error::InvalidArgument),
        }
    }
    pub fn execute(&self, action: Action) -> HostResult<Completion> {
        action.validate()?;
        let mut created = None;
        let output = match (self, action) {
            (Self::Directory { package, path }, Action::Open { path: child, mode }) => {
                if mode & FILE_WRITE != 0 {
                    return Err(Error::AccessDenied);
                }
                let child = std::str::from_utf8(&child).map_err(|_| Error::InvalidArgument)?;
                let path = if path.is_empty() { child.into() } else { format!("{path}/{child}") };
                if package.index().entry(&path).is_some() && mode == FILE_READ {
                    created = Some(Self::Reader { package: package.clone(), path });
                } else if package.index().is_directory(&path) && mode == FILE_DIRECTORY {
                    created = Some(Self::Directory { package: package.clone(), path });
                } else {
                    return Err(Error::NotFound);
                }
                Output::File { handle: 0 }
            }
            (Self::Reader { package, path }, Action::Read { offset, length }) => {
                let size = package.index().entry(path).ok_or(Error::Internal)?.length;
                let bytes = package
                    .read(path, offset.min(size), length as usize)
                    .map_err(|_| Error::Internal)?;
                Output::Read { bytes }
            }
            (Self::Directory { package, path }, Action::ListOpen) => {
                created = Some(Self::List {
                    package: package.clone(),
                    path: path.clone(),
                    entries: package.index().children(path).into(),
                });
                Output::List { handle: 0 }
            }
            (Self::List { package, path, entries }, Action::ListNext { index }) => {
                match entries
                    .get(index as usize)
                    .and_then(|entry| package.index().child(path, *entry))
                {
                    Some((name, directory)) => Output::Entry {
                        kind: if directory { FILE_KIND_DIRECTORY } else { FILE_KIND_FILE },
                        name: name.as_bytes().to_vec(),
                    },
                    None => Output::End,
                }
            }
            (_, Action::Status) => self.status()?.into(),
            (_, Action::Close { flags }) if valid_close_flags(flags, false) => {
                Output::Close { error: 0, publication: NOT_PUBLISHED, recovery: None }
            }
            (_, Action::Write { .. }) => return Err(Error::AccessDenied),
            _ => return Err(Error::InvalidArgument),
        };
        Ok(Completion { output, created })
    }
}
