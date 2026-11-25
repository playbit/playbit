//! Identity-bound browser metadata. Captures and selected content revisions are
//! retained independently of public handles and physical replica generations.
use super::*;
use crate::{
    runtime::{
        file_handles::Resource,
        host::files::{Action, Output},
    },
    store::{
        EntryType,
        volume_index::{BlobVersion, EntryAddress, EntryKind, IndexSnapshot},
    },
};
use playbit_api::abi::files::*;

#[derive(Clone)]
pub(super) struct Directory {
    pub(super) volume: Id,
    pub(super) parent: Option<EntryId>,
}
pub(super) enum Object {
    Bundle(Box<crate::package::filesystem::Object>),
    Directory(Directory),
    Writer(writer::Writer),
    Recovery { writer: Rc<Object>, engine: u64 },
    Reader { reference: blob::FileReference, status: FileStatus, _credit: readers::Credit },
    List { directory: Directory, entries: Vec<(u32, Vec<u8>)> },
}
#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Task {
    Done {
        #[serde(serialize_with = "output_json")]
        output: Output,
    },
    Read {
        reference: blob::FileReference,
    },
    CreateWriter {
        #[serde(serialize_with = "engine_json")]
        engine: u64,
    },
    Write {
        #[serde(serialize_with = "engine_json")]
        engine: u64,
        offset: u64,
    },
    PrivateRead {
        #[serde(serialize_with = "engine_json")]
        engine: u64,
        offset: u64,
        length: u32,
    },
    Close {
        #[serde(serialize_with = "engine_json")]
        engine: u64,
        flags: u32,
    },
}
impl Directory {
    pub(super) fn check(&self, host: &Host, retained: bool) -> Result<Rc<Volume>> {
        let volume = host.guest_volume(self.volume)?;
        let policy = host.admit_volume(&volume, Permission::Read)?;
        policy.authorize(host.actor(&volume), Permission::Read).map_err(store_error)?;
        authority::check(host, &volume, self.parent)?;
        let snapshot = IndexSnapshot::new(volume.index.begin_read().map_err(map_error)?);
        if !retained {
            snapshot.directory(self.parent).map_err(store_error)?;
        }
        if let Some(parent) = self.parent {
            snapshot
                .authorize_retained_read(
                    policy.volume(),
                    policy.default_permissions(),
                    host.actor(&volume),
                    parent,
                )
                .map_err(store_error)?;
        }
        drop(snapshot);
        Ok(volume)
    }
    fn open(
        &self,
        host: &Host,
        path: &[u8],
        mode: u32,
        budget: &Rc<RefCell<readers::Budget>>,
    ) -> Result<Object> {
        let path = std::str::from_utf8(path).map_err(|_| Error::InvalidArgument)?;
        let volume = self.check(host, false)?;
        let policy = host.admit_volume(&volume, Permission::Read)?;
        let snapshot = IndexSnapshot::new(volume.index.begin_read().map_err(map_error)?);
        let entry = snapshot.lookup_from(self.parent, path).map_err(store_error)?;
        authority::check(host, &volume, Some(entry.id))?;
        snapshot
            .authorize_namespace(
                policy.volume(),
                policy.default_permissions(),
                host.actor(&volume),
                entry.id,
                Permission::Read,
            )
            .map_err(store_error)?;
        match (mode, entry.info.kind) {
            (FILE_DIRECTORY, EntryKind::Directory {}) => {
                Ok(Object::Directory(Self { parent: Some(entry.id), ..self.clone() }))
            }
            (
                FILE_READ,
                EntryKind::Blob { len_bytes, digest, encrypted_key, published_at_micros },
            ) => {
                if len_bytes > MAX_FILE_EXTENT {
                    return Err(Error::LimitExceeded);
                }
                let version = BlobVersion {
                    address: EntryAddress { volume: volume.id, entry: entry.id },
                    revision: entry.content_revision,
                    len_bytes,
                    digest,
                    encrypted_key,
                };
                Ok(Object::Reader {
                    _credit: readers::Credit::reserve(budget, digest, len_bytes)?,
                    reference: blob::FileReference::from_version(version, 0, 0),
                    status: FileStatus {
                        kind: FILE_KIND_FILE,
                        flags: FILE_SIZE_VALID
                            | FILE_MODIFICATION_TIME_VALID
                            | FILE_CONTENTS_IMMUTABLE,
                        file_size: len_bytes,
                        modification_time: published_at_micros,
                        ..FileStatus::default()
                    },
                })
            }
            _ => Err(Error::InvalidArgument),
        }
    }
    fn list(&self, host: &Host) -> Result<Object> {
        let volume = self.check(host, false)?;
        let snapshot = IndexSnapshot::new(volume.index.begin_read().map_err(map_error)?);
        // children admits at most 16,384 entries and 4 MiB before each push.
        // The pending result already owns one of the eight listing credits.
        let entries = snapshot
            .children(self.parent)
            .map_err(store_error)?
            .into_iter()
            .map(|entry| {
                (
                    match entry.kind {
                        EntryType::Blob => FILE_KIND_FILE,
                        EntryType::Directory => FILE_KIND_DIRECTORY,
                        _ => FILE_KIND_OTHER,
                    },
                    entry.name.into_bytes(),
                )
            })
            .collect();
        Ok(Object::List { directory: self.clone(), entries })
    }
}
impl Object {
    pub(super) fn check(&self, host: &Host) -> Result<()> {
        match self {
            Self::Bundle(_) => Ok(()),
            Self::Directory(directory) => directory.check(host, false).map(|_| ()),
            Self::Reader { reference, .. } => {
                let volume = host.replica_volume(reference.volume)?;
                host.authorize_blob(&volume, &reference.version()?, false, false)
            }
            Self::List { directory, .. } => directory.check(host, true).map(|_| ()),
            Self::Writer(writer) => writer.check(host),
            Self::Recovery { writer, .. } => writer.check(host),
        }
    }
    fn status(&self, host: &Host) -> Result<Output> {
        self.check(host)?;
        match self {
            Self::Bundle(object) => Ok(object.status()?.into()),
            Self::Directory(_) => {
                Ok(FileStatus { kind: FILE_KIND_DIRECTORY, ..FileStatus::default() }.into())
            }
            Self::Reader { status, .. } => Ok((*status).into()),
            Self::List { .. } => Err(Error::InvalidArgument),
            Self::Writer(writer) => Ok(FileStatus::private(writer.size.get(), false).into()),
            Self::Recovery { writer, .. } => {
                let Self::Writer(writer) = writer.as_ref() else {
                    return Err(Error::Internal);
                };
                Ok(FileStatus::private(writer.size.get(), true).into())
            }
        }
    }
}
impl Files {
    pub(super) fn object(&self, source: &Resource) -> Result<Rc<Object>> {
        let engine = source.engine.ok_or(Error::Unsupported)?;
        self.objects.lookup(&engine).cloned().ok_or(Error::StaleHandle)
    }

    fn execute(&mut self, host: &Host, token: u64) -> Result<Task> {
        let result = self.execute_inner(host, token);
        if let Err(error) = &result
            && !matches!(error, Error::WouldBlock | Error::StaleHandle)
            && let Some(pending) = self.pending.lookup_mut(&token)
        {
            pending.terminal = true;
            if matches!(pending.action, Action::Write { .. })
                && let Object::Writer(writer) = pending.object.as_ref()
            {
                writer.failed.set(true);
            }
        }
        result
    }
    fn execute_inner(&mut self, host: &Host, token: u64) -> Result<Task> {
        self.check_order(token)?;
        let pending = self.pending.lookup_mut(&token).expect("checked pending operation");
        if pending.started || pending.terminal {
            return Err(Error::StaleHandle);
        }
        pending.started = true;
        let object = pending.object.clone();
        if let Object::Bundle(object) = object.as_ref() {
            return bundle_task(object, pending, &mut self.objects);
        }
        let output = match &pending.action {
            Action::Open { path, mode } => {
                let child = open_child(host, &object, pending, &self.readers, path, *mode)?;
                let engine = store_created(
                    &mut self.objects,
                    pending.created_slot,
                    &mut pending.created,
                    child,
                )?;
                if *mode & FILE_WRITE != 0 {
                    // The registry retains the stage credit after the last File
                    // or operation drops, until the worker confirms OPFS cleanup.
                    self.staged
                        .insert(engine, pending.stage.as_ref().ok_or(Error::Internal)?.clone());
                    return Ok(Task::CreateWriter { engine });
                }
                Output::File { handle: engine }
            }
            Action::ListOpen => {
                let Object::Directory(directory) = object.as_ref() else {
                    return Err(Error::InvalidArgument);
                };
                let list = directory.list(host)?;
                let engine = store_created(
                    &mut self.objects,
                    pending.created_slot,
                    &mut pending.created,
                    list,
                )?;
                Output::List { handle: engine }
            }
            Action::Status => object.status(host)?,
            Action::ListNext { index } => next(host, &object, *index)?,
            Action::Read { offset, length } => {
                return read_task(host, &object, pending, *offset, *length);
            }
            Action::Close { flags }
                if matches!(object.as_ref(), Object::Writer(_)) && *flags != FILE_CLOSE_DISCARD =>
            {
                return Ok(Task::Close {
                    engine: pending.source.engine.ok_or(Error::Internal)?,
                    flags: *flags,
                });
            }
            Action::Close { .. } => Output::Close { error: 0, publication: 0, recovery: None },
            Action::Write { offset, .. } => {
                return write_task(host, &object, pending, *offset);
            }
        };
        pending.output = Some(output.clone());
        pending.terminal = true;
        Ok(Task::Done { output })
    }
    fn check_order(&self, token: u64) -> Result<()> {
        self.check(token, false)?;
        let current = self.pending.lookup(&token).expect("checked pending operation");
        if self.pending.values().any(|earlier| {
            earlier.order < current.order
                && earlier.source.engine == current.source.engine
                && !earlier.terminal
        }) {
            return Err(Error::WouldBlock);
        }
        Ok(())
    }
}
fn store_created(
    objects: &mut HandleTable<Rc<Object>>,
    slot: Option<Handle>,
    created: &mut Option<(u64, Rc<Object>)>,
    object: Object,
) -> Result<u64> {
    let slot = slot.ok_or(Error::Internal)?;
    let object = Rc::new(object);
    objects.insert(slot, object.clone()).expect("operation owns its backend result reservation");
    let engine = slot.raw();
    *created = Some((engine, object));
    Ok(engine)
}
fn bundle_task(
    object: &crate::package::filesystem::Object,
    pending: &mut pending::Pending,
    objects: &mut HandleTable<Rc<Object>>,
) -> Result<Task> {
    let mut completion = object.execute(pending.action.clone())?;
    if let Some(created) = completion.created {
        let slot = pending.created_slot.ok_or(Error::Internal)?;
        let object = Rc::new(Object::Bundle(Box::new(created)));
        objects.insert(slot, object.clone()).expect("bundle operation owns result reservation");
        let engine = slot.raw();
        pending.created = Some((engine, object));
        match &mut completion.output {
            Output::File { handle } | Output::List { handle } => *handle = engine,
            _ => return Err(Error::Internal),
        }
    }
    pending.output = Some(completion.output.clone());
    pending.terminal = true;
    Ok(Task::Done { output: completion.output })
}
fn next(host: &Host, object: &Object, index: u32) -> Result<Output> {
    object.check(host)?;
    let Object::List { entries, .. } = object else {
        return Err(Error::InvalidArgument);
    };
    if index as usize > entries.len() {
        return Err(Error::InvalidArgument);
    }
    Ok(match entries.get(index as usize) {
        Some((kind, name)) => Output::Entry { kind: *kind, name: name.clone() },
        None => Output::End,
    })
}
#[unsafe(no_mangle)]
pub extern "C" fn pb_web_file_execute(token: u64) -> i32 {
    let mut length = 0;
    let status = operations::call(|host, _| {
        let task = host.files.borrow_mut().execute(host, token)?;
        let result = serde_json::to_vec(&task).map_err(map_error)?;
        length = result.len() as i32;
        *OUTPUT.lock().expect("mutex is not poisoned") = result;
        Ok(())
    });
    if status == 0 { length } else { status }
}
/// Hydration completed for the exact selected version. Never resolve the path
/// again; its current replacement does not change this retained read.
#[unsafe(no_mangle)]
pub extern "C" fn pb_web_file_read_complete(token: u64, length: u32) -> i32 {
    operations::call(|host, memory| {
        let mut files = host.files.borrow_mut();
        files.check(token, false)?;
        let pending = files.pending.lookup_mut(&token).expect("checked pending operation");
        let Action::Read { offset, length: maximum } = pending.action else {
            return Err(Error::InvalidArgument);
        };
        if !pending.started || pending.output.is_some() {
            return Err(Error::StaleHandle);
        }
        pending.object.check(host)?;
        let size = match pending.object.as_ref() {
            Object::Reader { reference, .. } => reference.len_bytes,
            Object::Recovery { writer, .. } => match writer.as_ref() {
                Object::Writer(writer) => writer.size.get(),
                _ => return Err(Error::Internal),
            },
            _ => return Err(Error::InvalidArgument),
        };
        let expected = u64::from(maximum).min(size.saturating_sub(offset));
        if u64::from(length) != expected {
            return Err(Error::Internal);
        }
        let data = memory.admit(8, length, 1, 1, MAX_FILE_READ)?;
        pending.output = Some(Output::Read { bytes: data.bytes(memory.data).to_vec() });
        pending.terminal = true;
        Ok(())
    })
}

impl Files {
    fn qualify(&mut self, host: &Host, token: u64) -> Result<()> {
        self.check(token, false)?;
        let pending = self.pending.lookup_mut(&token).expect("checked pending operation");
        if !pending.terminal {
            return Err(Error::WouldBlock);
        }
        if matches!(pending.action, Action::Close { .. }) {
            if let Some(Output::Close { recovery, .. }) = &mut pending.output
                && recovery.is_some()
            {
                let authority = self
                    .table
                    .authorize_resource(&pending.source, true)
                    .and_then(|()| pending.created.as_ref().ok_or(Error::Internal)?.1.check(host));
                if authority.is_err() {
                    // Preserve the actual publication outcome and keep the stage
                    // owned until cleanup; only the capability is withheld.
                    *recovery = None;
                }
            }
        } else {
            match pending.object.as_ref() {
                Object::Directory(directory) if !matches!(pending.action, Action::Status) => {
                    directory.check(host, true)?;
                }
                // Later failed writes cannot change an earlier captured result.
                // Authority remains live even though writer health is historical.
                Object::Writer(writer) => {
                    writer.target.staging_access(host)?;
                }
                object => object.check(host)?,
            }
            if let Some((_, created)) = &pending.created {
                created.check(host)?;
            }
        }
        pending.fence = Some((storage::revision(), self.revision));
        Ok(())
    }
}
/// Runs on the asynchronous worker, never inside a guest callback. Requalifying
/// a completed read preserves its selected revision and does not replay I/O.
#[unsafe(no_mangle)]
pub extern "C" fn pb_web_file_qualify(token: u64) -> i32 {
    operations::call(|host, _| host.files.borrow_mut().qualify(host, token))
}
#[unsafe(no_mangle)]
pub extern "C" fn pb_web_file_result(token: u64) -> i32 {
    let mut length = 0;
    let status = operations::call(|host, _| {
        let files = host.files.borrow();
        files.check(token, true)?;
        let output = files
            .pending
            .lookup(&token)
            .expect("checked pending operation")
            .output
            .as_ref()
            .ok_or(Error::WouldBlock)?;
        let result = serde_json::to_vec(&JsonOutput(output)).map_err(map_error)?;
        length = result.len() as i32;
        *OUTPUT.lock().expect("mutex is not poisoned") = result;
        Ok(())
    });
    if status == 0 { length } else { status }
}

/// The worker has retired a failed asynchronous read or private-stage creation.
/// Writes and normal closes use their own terminal-state transitions instead.
#[unsafe(no_mangle)]
pub extern "C" fn pb_web_file_io_failed(token: u64, status: i32) -> i32 {
    operations::call(|host, _| {
        let error = Error::try_from(status).map_err(|_| Error::InvalidArgument)?;
        if matches!(error, Error::WouldBlock | Error::BufferTooSmall) {
            return Err(Error::InvalidArgument);
        }
        let mut files = host.files.borrow_mut();
        let pending = files.pending.lookup_mut(&token).ok_or(Error::StaleHandle)?;
        if !pending.started || pending.terminal {
            return Err(Error::StaleHandle);
        }
        if !matches!(pending.action, Action::Open { .. } | Action::Read { .. }) {
            return Err(Error::InvalidArgument);
        }
        pending.terminal = true;
        Ok(())
    })
}

fn open_child(
    host: &Host,
    object: &Object,
    pending: &pending::Pending,
    readers: &Rc<RefCell<readers::Budget>>,
    path: &[u8],
    mode: u32,
) -> Result<Object> {
    let Object::Directory(directory) = object else {
        return Err(Error::InvalidArgument);
    };
    let child = if mode & FILE_WRITE != 0 {
        Object::Writer(writer::Writer {
            target: destination::Destination::open(
                host,
                directory,
                path,
                mode & FILE_EXCLUSIVE != 0,
            )?,
            size: Cell::new(0),
            failed: Cell::new(false),
            _credit: pending.stage.as_ref().ok_or(Error::Internal)?.clone(),
        })
    } else {
        directory.open(host, path, mode, readers)?
    };
    Ok(child)
}

fn read_task(
    host: &Host,
    object: &Object,
    _pending: &pending::Pending,
    offset: u64,
    length: u32,
) -> Result<Task> {
    object.check(host)?;
    if let Object::Recovery { engine, .. } = object {
        return Ok(Task::PrivateRead { engine: *engine, offset, length });
    }
    let Object::Reader { reference, .. } = object else {
        return Err(Error::InvalidArgument);
    };
    let mut reference = reference.clone();
    reference.offset = offset;
    reference.limit = length;
    Ok(Task::Read { reference })
}

fn write_task(
    host: &Host,
    object: &Object,
    pending: &pending::Pending,
    offset: u64,
) -> Result<Task> {
    if let Err(error) = object.check(host) {
        if let Object::Writer(writer) = object {
            writer.failed.set(true);
        }
        return Err(error);
    }
    Ok(Task::Write { engine: pending.source.engine.ok_or(Error::Internal)?, offset })
}
