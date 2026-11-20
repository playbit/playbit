//! Named blob operations and generation-scoped asynchronous selection/copy.
use super::{
    FileCopyRequest, HostError, HostResult, Opcode, OperationHandle, PendingOperation, State,
    VolumeRights, consume_operation, register_operation, require_supported, reserve_operation,
};
use playbit_api::ui::FileReference;

pub(in crate::runtime) fn address(
    state: &State,
    file: FileReference,
    rights: VolumeRights,
) -> HostResult<super::EntryAddress> {
    super::handles::entry_address(
        state,
        super::EntryHandle {
            volume: playbit_api::abi::dispatch::pack_handle(
                file.volume.generation,
                file.volume.slot,
            ),
            entry_id: file.entry_id,
            reserved: 0,
            ..super::EntryHandle::default()
        },
        rights,
    )
}

pub(in crate::runtime) fn copy_begin(
    state: &mut State,
    source: FileReference,
    destination: FileReference,
) -> HostResult<OperationHandle> {
    require_supported(state, Opcode::FileCopyBegin)?;
    let source_address = address(state, source, VolumeRights::READ)?;
    let destination_address = address(state, destination, VolumeRights::WRITE)?;
    if source_address == destination_address {
        return Err(HostError::InvalidArgument);
    }
    let slot = reserve_operation(state)?;
    let token = match state.host.begin_file_copy(
        u64::from(state.generation),
        FileCopyRequest { source: source_address, destination: destination_address },
    ) {
        Ok(token) => token,
        Err(error) => {
            super::handles::cancel_operation(state, slot);
            return Err(error);
        }
    };
    register_operation(
        state,
        slot,
        PendingOperation::FileCopy { token, destination_entry: destination.entry_id },
    )
}

pub(in crate::runtime) fn copy_poll(
    state: &mut State,
    handle: OperationHandle,
) -> HostResult<[u8; 15]> {
    require_supported(state, Opcode::FileCopyPoll)?;
    let pending = state.capabilities.operation(super::handles::raw_operation(handle))?.clone();
    let PendingOperation::FileCopy { token, destination_entry } = &pending else {
        return Err(HostError::StaleHandle);
    };
    let result = state.host.poll_file_copy(*token).and_then(|entry| {
        if entry == *destination_entry { Ok(entry) } else { Err(HostError::Internal) }
    });
    if result != Err(HostError::WouldBlock) {
        consume_operation(state, handle, pending);
    }
    result
}

pub(in crate::runtime) fn select_begin(state: &mut State) -> HostResult<OperationHandle> {
    require_supported(state, Opcode::FileSelectBegin)?;
    let slot = reserve_operation(state)?;
    let token = match state.host.begin_file_selection(u64::from(state.generation)) {
        Ok(token) => token,
        Err(error) => {
            super::handles::cancel_operation(state, slot);
            return Err(error);
        }
    };
    register_operation(state, slot, PendingOperation::FileSelection { token })
}

/// None is cancellation. A terminal result consumes its handle; WouldBlock retains it.
pub(in crate::runtime) fn select_poll(
    state: &mut State,
    handle: OperationHandle,
) -> HostResult<Option<FileReference>> {
    require_supported(state, Opcode::FileSelectPoll)?;
    let pending = state.capabilities.operation(super::handles::raw_operation(handle))?.clone();
    let PendingOperation::FileSelection { token } = &pending else {
        return Err(HostError::StaleHandle);
    };
    let result = state.host.poll_file_selection(*token).and_then(|selection| {
        let Some(selection) = selection else { return Ok(None) };
        if selection.address.entry_id == [0; 15] || selection.address.volume_id == [0; 16] {
            return Err(HostError::Internal);
        }
        let slot = state
            .context
            .volumes
            .iter()
            .position(|volume| volume.id == selection.address.volume_id)
            .ok_or(HostError::AccessDenied)?;
        let file = FileReference {
            volume: super::handles::volume_handle(state.volume_handles[slot]),
            entry_id: selection.address.entry_id,
        };
        address(state, file, VolumeRights::READ)?;
        Ok(Some(file))
    });
    if result != Err(HostError::WouldBlock) {
        consume_operation(state, handle, pending);
    }
    result
}
