//! Memory-only admission for asynchronous browser File operations. The pending
//! owner pins the source and reserves the destination before a close retires its
//! public handle. Backend I/O and guest delivery are separate transitions.
use super::*;
use crate::runtime::{
    file_handles::{Kind, Reservation, Resource},
    file_list::{self, Next},
    host::files::Action,
};
use playbit_api::abi::files::*;

pub(super) struct Pending {
    pub(super) action: Action,
    pub(super) order: u64,
    bytes: usize,
    pub(super) source: Resource,
    pub(super) object: Rc<objects::Object>,
    pub(super) created: Option<(u64, Rc<objects::Object>)>,
    pub(super) created_slot: Option<Handle>,
    pub(super) output: Option<runtime::Output>,
    pub(super) started: bool,
    pub(super) terminal: bool,
    pub(super) sealed: Option<blob::FileReference>,
    pub(super) stage: Option<Rc<writer::StageCredit>>,
    pub(super) fence: Option<(u64, u64)>,
    result: Option<Reservation>,
    next: Option<Next>,
    delivered: bool,
    created_claimed: bool,
}
fn cursor_error(error: file_list::Error) -> Error {
    match error {
        file_list::Error::Busy => Error::Busy,
        file_list::Error::Closed => Error::Closed,
        file_list::Error::LimitExceeded => Error::LimitExceeded,
    }
}
fn validate(source: &Resource, action: &Action) -> Result<()> {
    action.validate()?;
    let valid = match action {
        Action::Open { .. } | Action::ListOpen => source.kind == Kind::Directory,
        Action::Read { .. } => matches!(source.kind, Kind::Reader | Kind::Recovery),
        Action::Write { .. } => source.kind == Kind::Writer,
        Action::Status => source.kind != Kind::List,
        Action::Close { flags } => valid_close_flags(*flags, source.kind == Kind::Writer),
        Action::ListNext { .. } => source.kind == Kind::List,
    };
    if valid { Ok(()) } else { Err(Error::InvalidArgument) }
}
fn reserve(files: &mut Files, source: &Resource, action: &Action) -> Result<Option<Reservation>> {
    let kind = match action {
        Action::Open { mode: FILE_READ, .. } => Some(Kind::Reader),
        Action::Open { mode: FILE_DIRECTORY, .. } => Some(Kind::Directory),
        Action::Open { .. } => Some(Kind::Writer),
        Action::ListOpen => Some(Kind::List),
        Action::Close { flags } if source.kind == Kind::Writer && *flags != FILE_CLOSE_DISCARD => {
            return work::capabilities(|capabilities| files.table.recovery(capabilities, source))
                .map(Some);
        }
        _ => None,
    };
    kind.map(|kind| work::capabilities(|capabilities| files.table.reserve(capabilities, kind)))
        .transpose()
}
impl Files {
    fn begin(&mut self, file: FileHandle, mut action: Action) -> Result<u64> {
        if self.pending.len() >= 64 || self.next_pending == u64::MAX {
            return Err(Error::LimitExceeded);
        }
        let bytes = action.retained_bytes()?;
        if bytes > crate::runtime::host::MAX_RETAINED_DATA_BYTES - self.pending_bytes {
            return Err(Error::LimitExceeded);
        }
        let (source, next) = self.source(file, &mut action)?;
        validate(&source, &action)?;
        let required = action.capability_rights();
        let kind = if matches!(action, Action::ListNext { .. }) {
            crate::runtime::capabilities::LIST
        } else {
            crate::runtime::capabilities::FILE
        };
        work::capabilities(|capabilities| {
            capabilities.table.get(Handle(file.0), kind, required).map(|_| ())
        })?;
        let close = matches!(action, Action::Close { .. });
        if !close {
            self.table.authorize_resource(&source, writes(&action))?;
        }
        let stage = if matches!(action, Action::Open { mode, .. } if mode & FILE_WRITE != 0) {
            Some(writer::StageCredit::reserve(&self.stages)?)
        } else {
            None
        };
        let object = self.object(&source)?;
        let (result, created_slot, token) = self.reserve_results(&source, &action)?;
        let order = self.next_pending;
        self.next_pending += 1;
        self.pending
            .insert(
                token,
                Box::new(Pending {
                    order,
                    action,
                    bytes,
                    source,
                    object,
                    created: None,
                    created_slot,
                    output: None,
                    started: false,
                    terminal: false,
                    sealed: None,
                    stage,
                    fence: None,
                    result,
                    next,
                    delivered: false,
                    created_claimed: false,
                }),
            )
            .expect("new pending reservation remains owned");
        let token = token.raw();
        self.pending_bytes += bytes;
        if close {
            let source =
                work::capabilities(|capabilities| self.table.remove_file(capabilities, file))
                    .expect("validated handle remains owned until admission");
            if !source.alias
                && let Some(engine) = source.engine
            {
                self.objects.take(&engine);
            }
        }
        Ok(token)
    }
    fn source(&self, file: FileHandle, action: &mut Action) -> Result<(Resource, Option<Next>)> {
        if matches!(action, Action::ListNext { .. }) {
            let entry = work::capabilities(|capabilities| {
                self.table.list(capabilities, FileListHandle(file.0))
            })?;
            let next = entry
                .cursor
                .as_ref()
                .expect("list owns its cursor")
                .begin()
                .map_err(cursor_error)?;
            *action = Action::ListNext { index: next.index().map_err(cursor_error)? };
            Ok((entry.resource.clone(), Some(next)))
        } else {
            work::capabilities(|capabilities| self.table.file(capabilities, file))
                .map(|source| (source, None))
        }
    }
    fn reserve_results(
        &mut self,
        source: &Resource,
        action: &Action,
    ) -> Result<(Option<Reservation>, Option<Handle>, Handle)> {
        let result = reserve(self, source, action)?;
        let created_slot = if result.is_some() {
            match self.objects.reserve(1, Rights::ALL) {
                Ok(handle) => Some(handle),
                Err(error) => {
                    if let Some(result) = &result {
                        work::capabilities(|caps| {
                            result.cancel(caps);
                            Ok(())
                        })?;
                    }
                    return Err(error);
                }
            }
        } else {
            None
        };
        let token = match self.pending.reserve(1, Rights::NONE) {
            Ok(token) => token,
            Err(error) => {
                if let Some(slot) = created_slot {
                    self.objects.cancel(slot).expect("owned backend result reservation");
                }
                if let Some(result) = &result {
                    work::capabilities(|caps| {
                        result.cancel(caps);
                        Ok(())
                    })?;
                }
                return Err(error);
            }
        };
        Ok((result, created_slot, token))
    }
    pub(super) fn check(&self, token: u64, delivery: bool) -> Result<()> {
        let pending = self.pending.lookup(&token).ok_or(Error::StaleHandle)?;
        if pending.delivered {
            return Err(Error::StaleHandle);
        }
        if let Some(next) = &pending.next {
            next.index().map_err(cursor_error)?;
        }
        // A known close outcome survives revocation, but its optional recovery
        // capability needs current storage authority before any delivery.
        if matches!(pending.action, Action::Close { .. }) {
            let recovery =
                matches!(pending.output, Some(runtime::Output::Close { recovery: Some(_), .. }));
            if delivery && recovery && pending.fence != Some((storage::revision(), self.revision)) {
                return Err(Error::WouldBlock);
            }
            return Ok(());
        }
        self.table.authorize_resource(&pending.source, !delivery && writes(&pending.action))?;
        if delivery && pending.fence != Some((storage::revision(), self.revision)) {
            return Err(Error::WouldBlock);
        }
        Ok(())
    }
    fn finish(&mut self, token: u64, engine: u64, entry: bool) -> Result<FileHandle> {
        self.check(token, true)?;
        let pending = self.pending.lookup_mut(&token).expect("checked pending operation");
        let output = pending.output.as_ref().ok_or(Error::WouldBlock)?;
        let engine = if matches!(pending.action, Action::Close { .. })
            && self.table.authorize_resource(&pending.source, false).is_err()
        {
            0
        } else {
            engine
        };
        let expected = match output {
            runtime::Output::File { handle } | runtime::Output::List { handle } => *handle,
            runtime::Output::Close { recovery, .. } => recovery.unwrap_or(0),
            _ => 0,
        };
        let withheld = engine == 0
            && expected != 0
            && matches!(pending.action, Action::Close { .. })
            && self.table.authorize_resource(&pending.source, false).is_err();
        if (engine != expected && !withheld)
            || entry != matches!(output, runtime::Output::Entry { .. })
        {
            return Err(Error::InvalidArgument);
        }
        let capability = matches!(pending.action, Action::Open { .. } | Action::ListOpen);
        let recovery = matches!(pending.action, Action::Close { .. }) && pending.result.is_some();
        if (capability && engine == 0)
            || (engine != 0 && !capability && !recovery)
            || (entry && pending.next.is_none())
        {
            return Err(Error::InvalidArgument);
        }
        let handle = if engine != 0 {
            self.table.authorize_resource(&pending.source, false)?;
            let result = pending.result.as_ref().ok_or(Error::Internal)?;
            let (created, object) = pending.created.as_ref().ok_or(Error::Internal)?;
            if *created != engine {
                return Err(Error::Internal);
            }
            let handle = work::capabilities(|capabilities| {
                self.table.insert(capabilities, result, engine, pending.source.grant)
            })?;
            assert!(
                self.objects.lookup(&engine).is_some(),
                "created backend resource remains owned until delivery"
            );
            let _ = object;
            FileHandle(handle)
        } else {
            FileHandle::default()
        };
        if let Some(next) = &mut pending.next {
            next.take(entry, || true).expect("checked cursor cannot change during delivery");
        }
        pending.delivered = true;
        pending.created_claimed = engine != 0;
        Ok(handle)
    }
}
fn writes(action: &Action) -> bool {
    matches!(action, Action::Write { .. })
        || matches!(action, Action::Open { mode, .. } if mode & FILE_WRITE != 0)
}
fn action(kind: u32, offset: u64, flags: u32, bytes: &[u8], length: u32) -> Result<Action> {
    let action = match kind {
        8 => Action::Read { offset, length },
        9 => Action::Write { offset, bytes: bytes.to_vec() },
        23 => Action::Open { path: bytes.to_vec(), mode: flags },
        24 => Action::Status,
        25 => Action::Close { flags },
        26 if flags == 0 => Action::ListOpen,
        27 => Action::ListNext { index: 0 },
        _ => return Err(Error::InvalidArgument),
    };
    action.validate()?;
    Ok(action)
}
/// Called only after JavaScript has admitted guest memory and a Work slot.
/// The input payload starts at offset eight, as for other named host calls.
#[unsafe(no_mangle)]
pub extern "C" fn pb_web_file_begin(
    generation: u32,
    slot: u32,
    kind: u32,
    offset: u64,
    length: u32,
    flags: u32,
) -> i64 {
    let mut token = 0;
    let status = operations::call(|host, memory| {
        let size = if matches!(kind, 9 | 23) { length } else { 0 };
        let limit = if kind == 23 { MAX_FILE_PATH_BYTES as u32 } else { MAX_FILE_WRITE };
        let input = memory.admit(8, size, 1, 1, limit)?;
        let action = action(kind, offset, flags, input.bytes(memory.data), length)?;
        let mut files = host.files.borrow_mut();
        token = files.begin(FileHandle::new(generation, slot), action)?;
        let pending = &files.pending.lookup(&token).expect("checked pending operation");
        let mut output = OUTPUT.lock().expect("mutex is not poisoned");
        output.clear();
        output.extend_from_slice(&pending.source.engine.ok_or(Error::Unsupported)?.to_le_bytes());
        output.extend_from_slice(&u32::from(pending.source.alias).to_le_bytes());
        output.extend_from_slice(
            &files
                .roots
                .iter()
                .find(|root| root.admitted.grant == pending.source.grant)
                .map_or([0; 16], |root| *root.volume.as_bytes()),
        );
        Ok(())
    });
    if status == 0 { token as i64 } else { status as i64 }
}
#[unsafe(no_mangle)]
pub extern "C" fn pb_web_file_check(token: u64, delivery: u32) -> i32 {
    operations::call(|host, _| {
        if delivery > 1 {
            return Err(Error::InvalidArgument);
        }
        host.files.borrow().check(token, delivery != 0)
    })
}
#[unsafe(no_mangle)]
pub extern "C" fn pb_web_file_finish(token: u64, engine: u64, entry: u32) -> i32 {
    operations::call(|host, _| {
        if entry > 1 {
            return Err(Error::InvalidArgument);
        }
        let file = host.files.borrow_mut().finish(token, engine, entry != 0)?;
        let mut output = OUTPUT.lock().expect("mutex is not poisoned");
        output.clear();
        output.extend_from_slice(&file.0.to_le_bytes());
        Ok(())
    })
}
#[unsafe(no_mangle)]
pub extern "C" fn pb_web_file_release(token: u64) -> i32 {
    operations::call(|host, _| {
        let mut files = host.files.borrow_mut();
        let pending = files.pending.lookup(&token).ok_or(Error::StaleHandle)?;
        if !pending.terminal
            && (pending.started
                || matches!(pending.action, Action::Write { .. } | Action::Close { .. }))
        {
            return Err(Error::Busy);
        }
        let pending = files.pending.take(&token).expect("checked pending operation");
        files.pending_bytes -= pending.bytes;
        if let Some(slot) = pending.created_slot {
            if pending.created.is_none() {
                files.objects.cancel(slot).expect("unpublished backend result reservation");
            } else if !pending.created_claimed {
                files.objects.take(&slot.raw());
            }
        }
        if let Some(result) = &pending.result {
            work::capabilities(|caps| {
                result.cancel(caps);
                Ok(())
            })?;
        }
        Ok(())
    })
}
#[unsafe(no_mangle)]
pub extern "C" fn pb_web_file_list_close(generation: u32, slot: u32) -> i32 {
    operations::call(|host, _| {
        let entry = work::capabilities(|capabilities| {
            host.files
                .borrow_mut()
                .table
                .remove_list(capabilities, FileListHandle::new(generation, slot))
        })?;
        entry.cursor.as_ref().expect("list owns its cursor").close().map_err(cursor_error)?;
        if let Some(engine) = entry.resource.engine {
            host.files.borrow_mut().objects.take(&engine);
        }
        let mut output = OUTPUT.lock().expect("mutex is not poisoned");
        output.clear();
        output.extend_from_slice(&entry.resource.engine.ok_or(Error::Unsupported)?.to_le_bytes());
        Ok(())
    })
}
