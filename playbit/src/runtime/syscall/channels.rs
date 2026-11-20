//! Generation-scoped channel handles and guest event delivery; no persistent storage.
use super::*;
use crate::runtime::host::{DataOperation, DataResult};
use playbit_api::abi::dispatch::pack_handle;

pub(in crate::runtime) fn session(state: &mut State) -> HostResult<u32> {
    if state.context.identity.session_id.is_none() {
        return Err(HostError::Unsupported);
    }
    require_supported(state, Opcode::ChannelBegin)?;
    if state.channel_listener.is_none() {
        listen(state)?;
    }
    Ok(state.generation)
}
fn listen(state: &mut State) -> HostResult<()> {
    let call = std::sync::Arc::new(crate::runtime::host::DataCall {
        volume: [0; 16],
        operation: DataOperation::ChannelEvents { instance: state.context.identity.instance_id },
    });
    state.channel_listener = Some(state.host.begin_data(u64::from(state.generation), call)?);
    Ok(())
}
pub(in crate::runtime) fn open(state: &mut State, name: &str) -> HostResult<OperationHandle> {
    require_supported(state, Opcode::ChannelOpen)?;
    if state.context.identity.session_id.is_none() {
        return Err(HostError::Unsupported);
    }
    state
        .capabilities
        .open_channel(name)
        .map(|handle| super::handles::operation_handle(handle.raw()))
}
pub(in crate::runtime) fn begin(
    state: &mut State,
    channel: OperationHandle,
    action: u32,
    data: Vec<u8>,
) -> HostResult<OperationHandle> {
    let name = state
        .capabilities
        .channel_for_action(super::handles::raw_operation(channel), action)?
        .to_owned();
    let operation = DataOperation::ChannelCommand {
        instance: state.context.identity.instance_id,
        name,
        action,
        data,
    };
    let result = pending_data::begin(
        state,
        VolumeHandle { generation: state.generation, slot: 0 },
        operation,
        Opcode::ChannelBegin,
    )?;
    if action == 4 {
        state.capabilities.close_channel(super::handles::raw_operation(channel))?;
    }
    Ok(result)
}
pub(in crate::runtime) fn take(state: &mut State, operation: OperationHandle) -> HostResult<()> {
    pending_data::take(state, operation, Opcode::ChannelTake, |_, result| match result {
        DataResult::ChannelCompleted => Ok(()),
        _ => Err(HostError::Internal),
    })
}
/// Consume the internal receive without exposing a poll API to guests.
pub(in crate::runtime) fn receive(state: &mut State, token: u64) -> HostResult<bool> {
    if state.channel_listener != Some(token) {
        return Ok(false);
    }
    let received = state.host.peek_data(token);
    if matches!(received, Err(HostError::WouldBlock)) {
        return Err(HostError::WouldBlock);
    }
    let result = received.and_then(|result| match result.as_ref() {
        DataResult::ChannelEvents(events) => {
            crate::session_channel::validate_events(events).map_err(|_| HostError::Internal)?;
            Ok(events.clone())
        }
        _ => Err(HostError::Internal),
    });
    admit_delivery(state, &result)?;
    state.host.release_data(token);
    state.channel_listener = None;
    match result {
        Ok(events) => {
            for event in events {
                deliver(state, event)?;
            }
            listen(state)?;
        }
        Err(error) => {
            if error == HostError::Unavailable {
                state.set_participants(u64::from(state.generation), vec![])?;
            }
            push_event(
                state,
                Event {
                    kind: EventKind::ChannelStatus,
                    target: 0,
                    data: (error as i32).to_le_bytes().to_vec(),
                },
            )?;
            if error == HostError::Unavailable || error == HostError::LimitExceeded {
                listen(state)?;
            }
        }
    }
    Ok(true)
}
fn admit_delivery(
    state: &State,
    result: &HostResult<Vec<crate::session_channel::Event>>,
) -> HostResult<()> {
    // Keep the completed receive and its host credits until the guest drains
    // enough events. Ordinary readiness retries it without repeating I/O.
    let (count, bytes) = match result {
        Ok(events) => events.iter().fold((0, 0), |(count, bytes), event| match event {
            crate::session_channel::Event::Presence { .. } => (count + 1, bytes + 20),
            crate::session_channel::Event::Message { data, .. } => {
                (count + 1, bytes + 124 + data.len())
            }
        }),
        Err(_) => (2, 44),
    };
    if state.events.len() + count > MAX_QUEUED_EVENTS
        || state.queued_bytes + bytes > MAX_QUEUED_EVENT_BYTES
    {
        return Err(HostError::WouldBlock);
    }
    Ok(())
}
fn deliver(state: &mut State, event: crate::session_channel::Event) -> HostResult<()> {
    match event {
        crate::session_channel::Event::Presence { participants } => {
            let participants = participants
                .into_iter()
                .map(|p| {
                    let bytes = p.encode().map_err(|_| HostError::Internal)?;
                    Ok(Participant {
                        account_id: bytes[..16].try_into().expect("account"),
                        device_id: bytes[16..32].try_into().expect("device"),
                        instance_id: bytes[32..48].try_into().expect("instance"),
                        build_id: bytes[48..].try_into().expect("build"),
                    })
                })
                .collect::<HostResult<Vec<_>>>()?;
            state.set_participants(u64::from(state.generation), participants)
        }
        crate::session_channel::Event::Message { channel, sender, message_id, data } => {
            message(state, &channel, sender, &message_id, data)
        }
    }
}
fn message(
    state: &mut State,
    channel: &str,
    sender: crate::session_channel::Participant,
    message: &str,
    data: Vec<u8>,
) -> HostResult<()> {
    let message = crate::identifier::decode::<16>(message).map_err(|_| HostError::Internal)?;
    if let Some(handle) = state
        .capabilities
        .find_channel(channel)
        .map(|handle| super::handles::operation_handle(handle.raw()))
    {
        let mut payload = pack_handle(handle.generation, handle.slot).to_le_bytes().to_vec();
        payload.extend(sender.encode().map_err(|_| HostError::Internal)?);
        payload.extend(message);
        payload.extend(data);
        push_event(state, Event { kind: EventKind::ChannelMessage, target: 0, data: payload })
    } else {
        state.host.channel_consumed(
            state.context.identity.instance_id,
            crate::identifier::decode(&sender.account_id).map_err(|_| HostError::Internal)?,
            crate::identifier::decode(&sender.instance_id).map_err(|_| HostError::Internal)?,
            message,
        )
    }
}

pub(in crate::runtime) fn deactivate(state: &mut State) {
    if let Some(token) = state.channel_listener.take() {
        state.host.release_data(token);
    }
    state.host.retire_channels(state.context.identity.instance_id);
}
pub(in crate::runtime) fn activate(state: &mut State) {
    if state.context.identity.session_id.is_some()
        && state.host.supported_operations().contains(&Opcode::ChannelBegin)
        && let Err(error) = listen(state)
    {
        let _ = push_event(
            state,
            Event {
                kind: EventKind::ChannelStatus,
                target: 0,
                data: (error as i32).to_le_bytes().to_vec(),
            },
        );
    }
}
