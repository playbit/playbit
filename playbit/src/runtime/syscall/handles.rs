//! Scoped volume and pending-operation admission and retirement.
use super::{
    EntryAddress, EntryHandle, HostError, HostResult, Id, Opcode, OperationHandle,
    PendingOperation, State, VolumeHandle, VolumeRights, abi,
};

pub(in crate::runtime) fn entry_address(
    state: &State,
    handle: EntryHandle,
    rights: VolumeRights,
) -> HostResult<EntryAddress> {
    if !handle.valid() {
        return Err(HostError::InvalidArgument);
    }
    let volume_id = resolve_volume(state, volume_handle(handle.volume), rights)?;
    Ok(EntryAddress { volume_id, entry_id: handle.entry_id })
}

pub(in crate::runtime) fn resolve_volume(
    state: &State,
    handle: VolumeHandle,
    rights: VolumeRights,
) -> HostResult<Id> {
    let volume =
        state.capabilities.volume(raw_volume(handle), crate::capability::Rights(rights.0))?;
    if volume.backing == playbit_api::abi::data::VOLUME_BUNDLE {
        return Err(HostError::Unsupported);
    }
    Ok(volume.id)
}
pub(in crate::runtime) fn raw_volume(handle: VolumeHandle) -> crate::capability::Handle {
    crate::capability::Handle::new(handle.generation, handle.slot)
}
pub(in crate::runtime) fn raw_operation(handle: OperationHandle) -> crate::capability::Handle {
    crate::capability::Handle::new(handle.generation, handle.slot)
}
pub(in crate::runtime) fn operation_handle(raw: u64) -> OperationHandle {
    let handle = crate::capability::Handle(raw);
    OperationHandle { generation: handle.generation(), slot: handle.index() }
}
pub(in crate::runtime) fn volume_handle(raw: u64) -> VolumeHandle {
    let handle = crate::capability::Handle(raw);
    VolumeHandle { generation: handle.generation(), slot: handle.index() }
}
pub(in crate::runtime) fn reserve_operation(state: &mut State) -> HostResult<u64> {
    state.reap_work();
    let admission = state.work.admit_step();
    if state.work.draining && admission.is_err() {
        state.work.failed = true;
    }
    admission?;
    if state.capabilities.count(crate::runtime::capabilities::OPERATION)
        >= abi::MAX_PENDING_OPERATIONS
    {
        return Err(HostError::LimitExceeded);
    }
    state
        .capabilities
        .reserve(crate::runtime::capabilities::OPERATION, crate::capability::Rights::NONE)
        .map(|handle| handle.raw())
}
pub(in crate::runtime) fn cancel_operation(state: &mut State, raw: u64) {
    state
        .capabilities
        .cancel(crate::capability::Handle(raw))
        .expect("caller owns operation reservation");
}
pub(in crate::runtime) fn register_operation(
    state: &mut State,
    raw: u64,
    pending: PendingOperation,
) -> HostResult<OperationHandle> {
    if state.work.pending.keys().any(|raw| {
        let current = state
            .capabilities
            .operation(crate::capability::Handle(*raw))
            .expect("accepted work retains its operation capability");
        current.token().is_some()
            && current.token() == pending.token()
            && (current.ready_kind() == pending.ready_kind()
                || matches!(
                    (current, &pending),
                    (PendingOperation::Data { .. }, PendingOperation::Data { .. })
                ))
    }) {
        cancel_operation(state, raw);
        return Err(HostError::Internal);
    }
    state.capabilities.insert_operation(crate::capability::Handle(raw), pending)?;
    state.work.register(raw);
    Ok(operation_handle(raw))
}
pub(in crate::runtime) fn consume_operation(
    state: &mut State,
    handle: OperationHandle,
    pending: PendingOperation,
) {
    let raw = raw_operation(handle);
    let request = raw.raw().to_le_bytes();
    state.work.active =
        Some(*state.work.pending.get(&raw.raw()).expect("completion retains its accepted root"));
    if let PendingOperation::Data { token, bytes, .. } = &pending {
        assert!(state.pending_data_bytes >= *bytes);
        state.pending_data_bytes -= *bytes;
        state.host.release_data(*token);
    }
    if let PendingOperation::File(pending) = &pending {
        assert!(state.pending_data_bytes >= pending.bytes);
        state.pending_data_bytes -= pending.bytes;
        pending.cancel_result(&mut state.capabilities);
    }
    #[cfg(all(feature = "native-audio", target_vendor = "apple"))]
    if let PendingOperation::AudioOpen(open) = &pending {
        open.cancel_result(&mut state.capabilities);
    }
    state.capabilities.remove_operation(raw).expect("consuming owned operation");
    state.work.progress = true;
    assert!(
        state.work.pending.remove(&raw.raw()).is_some(),
        "accepted operation owns its work root"
    );
    state.events.retain(|queued| {
        let consumed = queued.event.kind == pending.ready_kind() && queued.event.data == request;
        if consumed {
            state.queued_bytes -= queued.event.data.len() + 20;
        }
        !consumed
    });
}

/// The adapter admits fixed output before taking a retained creation receipt.
pub(in crate::runtime) fn poll_operation(
    state: &mut State,
    handle: OperationHandle,
    opcode: Opcode,
) -> HostResult<Id> {
    if opcode != Opcode::ObjectCreatePoll {
        return Err(HostError::Unsupported);
    }
    super::pending_data::take(state, handle, opcode, |_, result| match result {
        crate::runtime::host::DataResult::Created(id) => Ok(*id),
        _ => Err(HostError::Internal),
    })
}
