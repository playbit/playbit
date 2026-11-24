//! Begin owns input; take reads one retained completion and never repeats I/O.
use super::{
    HostError, HostResult, Opcode, OperationHandle, PendingOperation, State, VolumeHandle,
    consume_operation, register_operation, require_supported, reserve_operation, resolve_volume,
};
use crate::runtime::host::{DataCall, DataOperation, DataResult, MAX_RETAINED_DATA_BYTES};
use std::sync::Arc;

pub(in crate::runtime) fn admit_begin(
    state: &State,
    volume: VolumeHandle,
    opcode: Opcode,
) -> HostResult<()> {
    require_supported(state, opcode)?;
    if opcode == Opcode::ChannelBegin {
        return if state.context.identity.session_id.is_some() {
            Ok(())
        } else {
            Err(HostError::Unsupported)
        };
    }
    let rights = match opcode {
        Opcode::KvKeysBegin
        | Opcode::BloblogReadBegin
        | Opcode::KvReadBegin
        | Opcode::EntryResolveBegin
        | Opcode::ObjectReadBegin
        | Opcode::ObjectQueryBegin
        | Opcode::SchemaReadBegin
        | Opcode::ObjectFieldReadAuthenticatedBegin => super::VolumeRights::READ,
        Opcode::SchemaEstablishBegin => super::VolumeRights::SCHEMA,
        Opcode::KvPutBegin | Opcode::KvDeleteBegin => super::VolumeRights::KV_WRITE,
        Opcode::BloblogAppendBegin | Opcode::ObjectEditBegin | Opcode::ObjectCreateBegin => {
            super::VolumeRights::WRITE
        }
        _ => return Err(HostError::Unsupported),
    };
    resolve_volume(state, volume, rights).map(|_| ())
}

pub(in crate::runtime) fn admit_entry(
    state: &State,
    entry: super::EntryHandle,
    opcode: Opcode,
) -> HostResult<()> {
    if !entry.valid() {
        return Err(HostError::InvalidArgument);
    }
    admit_begin(state, super::handles::volume_handle(entry.volume), opcode)
}

pub(in crate::runtime) fn begin_entry(
    state: &mut State,
    entry: super::EntryHandle,
    operation: DataOperation,
    opcode: Opcode,
) -> HostResult<OperationHandle> {
    admit_entry(state, entry, opcode)?;
    if operation.entry_id() != Some(entry.entry_id) {
        return Err(HostError::InvalidArgument);
    }
    begin(state, super::handles::volume_handle(entry.volume), operation, opcode)
}

pub(in crate::runtime) fn resolve_entry_take(
    state: &mut State,
    handle: OperationHandle,
) -> HostResult<super::EntryHandle> {
    admit_take(state, handle, Opcode::EntryResolveTake)?;
    let Some(PendingOperation::Data { volume, .. }) =
        state.capabilities.operation(super::handles::raw_operation(handle)).ok()
    else {
        return Err(HostError::StaleHandle);
    };
    let volume = playbit_api::abi::dispatch::pack_handle(volume.generation, volume.slot);
    take(state, handle, Opcode::EntryResolveTake, |_, result| match result {
        DataResult::EntryResolved(entry_id) => Ok(super::EntryHandle {
            volume,
            entry_id: *entry_id,
            reserved: 0,
            ..super::EntryHandle::default()
        }),
        _ => Err(HostError::Internal),
    })
}

/// Validate logical identity before the adapter reads pointed-to descriptors.
/// Authority denial is handled by take, after output admission, and consumes it.
pub(in crate::runtime) fn admit_take(
    state: &State,
    handle: OperationHandle,
    opcode: Opcode,
) -> HostResult<()> {
    require_supported(state, opcode)?;
    match state.capabilities.operation(super::handles::raw_operation(handle)).ok() {
        Some(PendingOperation::Data { take, .. }) if *take == opcode => Ok(()),
        _ => Err(HostError::StaleHandle),
    }
}

pub(in crate::runtime) fn begin(
    state: &mut State,
    volume: VolumeHandle,
    operation: DataOperation,
    opcode: Opcode,
) -> HostResult<OperationHandle> {
    let take = match (&operation, opcode) {
        (
            DataOperation::ChannelCommand { .. } | DataOperation::ChannelEvents { .. },
            Opcode::ChannelBegin,
        ) => Opcode::ChannelTake,
        (DataOperation::KvKeys { .. }, Opcode::KvKeysBegin) => Opcode::KvKeysTake,
        (DataOperation::KvRead { .. }, Opcode::KvReadBegin) => Opcode::KvReadTake,
        (DataOperation::KvPut { .. }, Opcode::KvPutBegin) => Opcode::KvPutTake,
        (DataOperation::KvDelete { .. }, Opcode::KvDeleteBegin) => Opcode::KvDeleteTake,
        (DataOperation::BloblogAppend { .. }, Opcode::BloblogAppendBegin) => {
            Opcode::BloblogAppendTake
        }
        (DataOperation::BloblogRead { .. }, Opcode::BloblogReadBegin) => Opcode::BloblogReadTake,
        (DataOperation::ResolveEntry { .. }, Opcode::EntryResolveBegin) => Opcode::EntryResolveTake,
        (DataOperation::SchemaRead { .. }, Opcode::SchemaReadBegin) => Opcode::SchemaReadTake,
        (DataOperation::SchemaEstablish { .. }, Opcode::SchemaEstablishBegin) => {
            Opcode::SchemaEstablishTake
        }
        (DataOperation::Create { .. }, Opcode::ObjectCreateBegin) => Opcode::ObjectCreatePoll,
        (DataOperation::Read { .. }, Opcode::ObjectReadBegin) => Opcode::ObjectReadTake,
        (DataOperation::Edit { .. }, Opcode::ObjectEditBegin) => Opcode::ObjectEditTake,
        (DataOperation::Query { .. }, Opcode::ObjectQueryBegin) => Opcode::ObjectQueryTake,
        (DataOperation::AuthenticatedField { .. }, Opcode::ObjectFieldReadAuthenticatedBegin) => {
            Opcode::ObjectFieldReadAuthenticatedTake
        }
        _ => return Err(HostError::Unsupported),
    };
    admit_begin(state, volume, opcode)?;
    require_supported(state, take)?;
    let volume_id = if operation.network() {
        [0; 16]
    } else {
        resolve_volume(state, volume, operation.rights())?
    };
    operation.validate(state.query_limits)?;
    let call = Arc::new(DataCall { volume: volume_id, operation });
    let bytes = call.retained_bytes().ok_or(HostError::LimitExceeded)?;
    if !credit_fits(state.pending_data_bytes, bytes, MAX_RETAINED_DATA_BYTES) {
        return Err(HostError::LimitExceeded);
    }
    let slot = reserve_operation(state)?;
    let token = match state.host.begin_data(u64::from(state.generation), Arc::clone(&call)) {
        Ok(token) => token,
        Err(error) => {
            super::handles::cancel_operation(state, slot);
            return Err(error);
        }
    };
    let handle = register_operation(
        state,
        slot,
        PendingOperation::Data { token, volume, call, take, bytes },
    );
    if handle.is_ok() {
        state.pending_data_bytes += bytes;
    } else {
        // A duplicate token is a host contract failure. Releasing it here
        // could consume the older operation which owns that token.
        state.halted = true;
    }
    handle
}

/// Pointer admission precedes this call. The adapter changes only counts on a
/// short buffer. A wrong kind/generation cannot inspect or consume another op.
pub(in crate::runtime) fn take<T>(
    state: &mut State,
    handle: OperationHandle,
    opcode: Opcode,
    output: impl FnOnce(&DataCall, &DataResult) -> HostResult<T>,
) -> HostResult<T> {
    admit_take(state, handle, opcode)?;
    let pending = state.capabilities.operation(super::handles::raw_operation(handle))?.clone();
    let PendingOperation::Data { token, volume, call, take, .. } = &pending else {
        return Err(HostError::StaleHandle);
    };
    if *take != opcode {
        return Err(HostError::StaleHandle);
    }
    let result = (|| {
        if !call.operation.is_mutation() && !call.operation.network() {
            resolve_volume(state, *volume, call.operation.rights())?;
        }
        let result = state.host.peek_data(*token)?;
        let bytes = result.retained_bytes().ok_or(HostError::LimitExceeded)?;
        if !credit_fits(state.pending_data_bytes, bytes, MAX_RETAINED_DATA_BYTES) {
            return Err(HostError::LimitExceeded);
        }
        call.validate_result(&result, state.query_limits)?;
        if call.operation.is_mutation() && !result.is_receipt(call.operation.kind()) {
            return Err(HostError::Internal);
        }
        output(call, &result)
    })();
    if !matches!(result, Err(HostError::WouldBlock | HostError::BufferTooSmall)) {
        consume_operation(state, handle, pending);
    }
    result
}

fn credit_fits(used: usize, bytes: usize, limit: usize) -> bool {
    used <= limit && bytes <= limit - used
}

#[cfg(test)]
#[path = "pending_data_tests.rs"]
pub(in crate::runtime) mod tests;
