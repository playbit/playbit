//! Shared File Begin/Take ownership. Adapters admit output memory before calling
//! these functions; a failed buffer check never consumes a cursor or result.
use super::{
    HostError, HostResult, Opcode, OperationHandle, PendingOperation, State, consume_operation,
    register_operation, require_supported, reserve_operation,
};
use crate::runtime::{
    file_handles::{Kind, Reservation, Resource},
    file_list::{self, Next},
    host::{
        MAX_RETAINED_DATA_BYTES,
        files::{Action, Call, NOT_PUBLISHED, Output, capacity::Retention},
    },
};
use playbit_api::abi::files::*;
use std::{cell::RefCell, rc::Rc, sync::Arc};

#[derive(Clone)]
pub(in crate::runtime) struct Pending {
    pub token: Option<u64>,
    pub bytes: usize,
    call: Arc<Call>,
    source: Resource,
    result: Option<Reservation>,
    take: Opcode,
    local: Option<Arc<Output>>,
    next: Option<Rc<RefCell<Next>>>,
}
impl Pending {
    pub(in crate::runtime) fn cancel_result<P>(
        &self,
        caps: &mut crate::runtime::capabilities::Capabilities<P>,
    ) {
        if let Some(result) = &self.result {
            result.cancel(caps);
        }
    }
}
fn cursor_error(error: file_list::Error) -> HostError {
    match error {
        file_list::Error::Busy => HostError::Busy,
        file_list::Error::Closed => HostError::Closed,
        file_list::Error::LimitExceeded => HostError::LimitExceeded,
    }
}
fn operation(action: &Action) -> (Opcode, Opcode) {
    match action {
        Action::Open { .. } => (Opcode::FileOpenBegin, Opcode::FileOpenTake),
        Action::Read { .. } => (Opcode::FileReadBegin, Opcode::FileReadTake),
        Action::Write { .. } => (Opcode::FileWriteBegin, Opcode::FileWriteTake),
        Action::Status => (Opcode::FileStatusBegin, Opcode::FileStatusTake),
        Action::Close { .. } => (Opcode::FileCloseBegin, Opcode::FileCloseTake),
        Action::ListOpen => (Opcode::FileListOpenBegin, Opcode::FileListOpenTake),
        Action::ListNext { .. } => (Opcode::FileListNextBegin, Opcode::FileListNextTake),
    }
}
fn validate(source: &Resource, action: &Action) -> HostResult<()> {
    action.validate()?;
    let valid = match action {
        Action::Open { .. } | Action::ListOpen => source.kind == Kind::Directory,
        Action::Read { .. } => matches!(source.kind, Kind::Reader | Kind::Recovery),
        Action::Write { .. } => source.kind == Kind::Writer,
        Action::Status => source.kind != Kind::List,
        Action::Close { flags } => valid_close_flags(*flags, source.kind == Kind::Writer),
        Action::ListNext { .. } => source.kind == Kind::List,
    };
    if valid { Ok(()) } else { Err(HostError::InvalidArgument) }
}
fn result(
    state: &mut State,
    source: &Resource,
    action: &Action,
) -> HostResult<Option<Reservation>> {
    let kind = match action {
        Action::Open { mode, .. } => Some(match *mode {
            FILE_READ => Kind::Reader,
            FILE_DIRECTORY => Kind::Directory,
            _ => Kind::Writer,
        }),
        Action::ListOpen => Some(Kind::List),
        Action::Close { flags } if source.kind == Kind::Writer && *flags != FILE_CLOSE_DISCARD => {
            return state.files.recovery(&mut state.capabilities, source).map(Some);
        }
        _ => None,
    };
    kind.map(|kind| state.files.reserve(&mut state.capabilities, kind)).transpose()
}
pub(in crate::runtime) fn begin(
    state: &mut State,
    file: FileHandle,
    action: Action,
) -> HostResult<OperationHandle> {
    let source = state.files.file(&state.capabilities, file)?.clone();
    validate(&source, &action)?;
    state.capabilities.table.get(
        crate::capability::Handle(file.0),
        crate::runtime::capabilities::FILE,
        action.capability_rights(),
    )?;
    if !matches!(action, Action::Close { .. }) {
        state.files.authorize_resource(
            &source,
            action.capability_rights().contains(crate::capability::Rights::WRITE),
        )?;
    }
    let close = matches!(action, Action::Close { .. });
    let pending = admit(state, source, action, None)?;
    if close {
        state.files.remove_file(&mut state.capabilities, file)?;
    }
    Ok(pending)
}
pub(in crate::runtime) fn list_next(
    state: &mut State,
    list: FileListHandle,
) -> HostResult<OperationHandle> {
    let entry = state.files.list(&state.capabilities, list)?;
    state.capabilities.table.get(
        crate::capability::Handle(list.0),
        crate::runtime::capabilities::LIST,
        crate::capability::Rights::READ,
    )?;
    let source = entry.resource.clone();
    state.files.authorize_resource(&source, false)?;
    let next =
        entry.cursor.as_ref().expect("FileList owns one cursor").begin().map_err(cursor_error)?;
    let index = next.index().map_err(cursor_error)?;
    admit(state, source, Action::ListNext { index }, Some(Rc::new(RefCell::new(next))))
}
fn local_capacity(state: &State) -> HostResult<()> {
    if state.events.len() == super::MAX_QUEUED_EVENTS
        || state.queued_bytes + 8 + playbit_api::abi::syscall::EVENT_QUEUE_OVERHEAD
            > super::MAX_QUEUED_EVENT_BYTES
    {
        return Err(HostError::LimitExceeded);
    }
    Ok(())
}
fn notify_local(state: &mut State, handle: OperationHandle) {
    let raw = super::handles::raw_operation(handle).raw();
    let data = raw.to_le_bytes().to_vec();
    let root = state.work.pending[&raw];
    super::events::push_work_event(
        state,
        super::Event { object_id: 0, kind: super::EventKind::DataReady, target: 0, data },
        Some(root),
    )
    .expect("local completion capacity admitted before changing ownership");
}
fn admit(
    state: &mut State,
    source: Resource,
    action: Action,
    next: Option<Rc<RefCell<Next>>>,
) -> HostResult<OperationHandle> {
    let (begin, take) = operation(&action);
    require_supported(state, begin)?;
    require_supported(state, take)?;
    let bytes = action.retained_bytes()?;
    if bytes > MAX_RETAINED_DATA_BYTES - state.pending_data_bytes {
        return Err(HostError::LimitExceeded);
    }
    let local = source.alias && matches!(action, Action::Close { .. });
    if local {
        local_capacity(state)?;
    }
    let engine = if local {
        source.engine.unwrap_or(0)
    } else {
        source.engine.ok_or(HostError::Unsupported)?
    };
    let slot = reserve_operation(state)?;
    let result = match result(state, &source, &action) {
        Ok(result) => result,
        Err(error) => {
            super::handles::cancel_operation(state, slot);
            return Err(error);
        }
    };
    let call = Arc::new(Call { file: engine, action });
    let token = start(state, slot, &call, &source, result.as_ref(), local)?;
    let pending = Pending {
        token,
        bytes,
        call,
        source,
        result,
        take,
        next,
        local: local.then(|| {
            Arc::new(Output::Close { error: 0, publication: NOT_PUBLISHED, recovery: None })
        }),
    };
    let handle = register_operation(state, slot, PendingOperation::File(pending.clone()));
    if let Ok(handle) = handle {
        state.pending_data_bytes += bytes;
        if local {
            notify_local(state, handle);
        }
    } else {
        pending.cancel_result(&mut state.capabilities);
        // A duplicate host token belongs to the older operation. Preserve it
        // until fail-stop teardown rather than release another call's result.
        state.halted = true;
    }
    handle
}
fn start(
    state: &mut State,
    slot: u64,
    call: &Arc<Call>,
    source: &Resource,
    result: Option<&Reservation>,
    local: bool,
) -> HostResult<Option<u64>> {
    if local {
        return Ok(None);
    }
    match state.host.begin_file(
        u64::from(state.generation),
        call.clone(),
        Retention { source: Some(source.credit.clone()), result: result.map(Reservation::credit) },
    ) {
        Ok(token) => Ok(Some(token)),
        Err(error) => {
            if let Some(result) = result {
                result.cancel(&mut state.capabilities);
            }
            super::handles::cancel_operation(state, slot);
            Err(error)
        }
    }
}
pub(in crate::runtime) fn admit_take(
    state: &State,
    handle: OperationHandle,
    opcode: Opcode,
) -> HostResult<()> {
    require_supported(state, opcode)?;
    match state.capabilities.operation(super::handles::raw_operation(handle)).ok() {
        Some(PendingOperation::File(pending)) if pending.take == opcode => Ok(()),
        _ => Err(HostError::StaleHandle),
    }
}
/// `slot` is reserved before Begin, and is transferred only if output succeeds.
/// In particular a short listing buffer changes no handle or cursor ownership.
pub(in crate::runtime) fn take<T>(
    state: &mut State,
    handle: OperationHandle,
    opcode: Opcode,
    output: impl FnOnce(&Output, Option<u64>) -> HostResult<T>,
) -> HostResult<T> {
    admit_take(state, handle, opcode)?;
    let pending = state.capabilities.operation(super::handles::raw_operation(handle))?.clone();
    let PendingOperation::File(file) = &pending else { unreachable!() };
    let mut claimed = false;
    let result = (|| {
        let close = matches!(file.call.action, Action::Close { .. });
        if !close {
            state.files.authorize_resource(&file.source, false)?;
        }
        if let Some(next) = &file.next {
            next.borrow().index().map_err(cursor_error)?;
        }
        let response = match file.token {
            Some(token) => state.host.peek_file(token)?,
            None => file.local.clone().expect("local close retains its receipt"),
        };
        file.call.validate_output(&response)?;
        let withheld;
        let response = if close
            && state.files.authorize_resource(&file.source, false).is_err()
            && let Output::Close { error, publication, recovery: Some(_) } = response.as_ref()
        {
            withheld = Output::Close { error: *error, publication: *publication, recovery: None };
            &withheld
        } else {
            response.as_ref()
        };
        let (value, delivered) = deliver(state, file, response, output)?;
        claimed = delivered;
        if let Some(next) = &file.next {
            next.borrow_mut()
                .take(matches!(response, Output::Entry { .. }), || true)
                .map_err(cursor_error)?;
        }
        Ok(value)
    })();
    if !matches!(result, Err(HostError::WouldBlock | HostError::BufferTooSmall)) {
        if let Some(token) = file.token {
            state.host.release_file(token, claimed);
        }
        consume_operation(state, handle, pending);
    }
    result
}
fn deliver<T>(
    state: &mut State,
    file: &Pending,
    response: &Output,
    output: impl FnOnce(&Output, Option<u64>) -> HostResult<T>,
) -> HostResult<(T, bool)> {
    let capability = match response {
        Output::File { handle } | Output::List { handle } => Some(*handle),
        Output::Close { recovery, .. } => *recovery,
        _ => None,
    };
    let slot = capability
        .map(|_| file.result.as_ref().ok_or(HostError::Internal).map(Reservation::handle))
        .transpose()?;
    let value = output(response, slot)?;
    if let Some(engine) = capability {
        state
            .files
            .insert(
                &mut state.capabilities,
                file.result.as_ref().expect("validated result reservation"),
                engine,
                file.source.grant,
            )
            .expect("validated authority and unused slot remain stable during bounded delivery");
    }
    Ok((value, capability.is_some()))
}
pub(in crate::runtime) fn dispose(state: &mut State, handle: OperationHandle) -> HostResult<()> {
    require_supported(state, Opcode::OperationDispose)?;
    let pending = state.capabilities.operation(super::handles::raw_operation(handle))?.clone();
    match &pending {
        PendingOperation::File(file) => {
            if let Some(token) = file.token {
                state.host.release_file(token, false);
            }
        }
        PendingOperation::Data { .. } => {}
        #[cfg(all(feature = "native-audio", target_vendor = "apple"))]
        PendingOperation::AudioOpen(open) => open.cancel(),
        _ => return Err(HostError::Unsupported),
    }
    consume_operation(state, handle, pending);
    Ok(())
}
/// Drop cleanup has reserved transport capacity and consumes no operation slot.
/// It discards an unclosed writer; an accepted close already removed its handle.
pub(in crate::runtime) fn discard(state: &mut State, file: FileHandle) -> HostResult<()> {
    require_supported(state, Opcode::FileDispose)?;
    let resource = state.files.file(&state.capabilities, file)?;
    if !resource.alias {
        state.host.discard_file(
            resource.engine.ok_or(HostError::Unsupported)?,
            false,
            Retention { source: Some(resource.credit.clone()), result: None },
        )?;
    }
    state.files.remove_file(&mut state.capabilities, file)?;
    Ok(())
}
pub(in crate::runtime) fn list_close(state: &mut State, list: FileListHandle) -> HostResult<()> {
    require_supported(state, Opcode::FileListClose)?;
    let entry = state.files.list(&state.capabilities, list)?;
    let engine = entry.resource.engine.ok_or(HostError::Unsupported)?;
    let pending = state.capabilities.operations().find_map(|(slot, pending)| match pending {
        PendingOperation::File(file)
            if file.source.engine == Some(engine) && file.next.is_some() =>
        {
            Some(slot.raw())
        }
        _ => None,
    });
    if pending.is_some() {
        local_capacity(state)?;
    }
    let entry = state.files.remove_list(&mut state.capabilities, list)?;
    entry.cursor.as_ref().expect("FileList owns its cursor").close().map_err(cursor_error)?;
    let result = state.host.discard_file(
        engine,
        true,
        Retention { source: Some(entry.resource.credit.clone()), result: None },
    );
    if result.is_err() {
        state.halted = true;
    }
    if let Some(slot) = pending {
        notify_local(state, super::handles::operation_handle(slot));
    }
    result
}

#[cfg(test)]
#[path = "pending_files_tests.rs"]
mod tests;
