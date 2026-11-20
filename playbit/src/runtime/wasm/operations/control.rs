use super::{WasmState, call};
use crate::runtime::linear_memory::layout::operations::{
    event as layout_event, identity as layout_identity,
};
use crate::runtime::syscall::{HostError, control, environment, events};
use playbit_api::abi::syscall as contract;
use wasmtime::Caller;

pub(super) fn log(caller: Caller<'_, WasmState>, pointer: u32, length: u32) -> i64 {
    call(caller, |state, memory| {
        let input = memory.admit(pointer, length, 1, 1, u32::MAX)?;
        let text = std::str::from_utf8(input.bytes(memory.data))
            .map_err(|_| HostError::InvalidArgument)?;
        control::log(state, text).map(|()| 0)
    })
}
pub(super) fn view_submit(caller: Caller<'_, WasmState>, pointer: u32, length: u32) -> i64 {
    call(caller, |state, memory| {
        let input = memory.admit(pointer, length, 1, 1, state.view_bytes_limit)?;
        control::view_submit(state, input.bytes(memory.data)).map(|()| 0)
    })
}
pub(super) fn event_poll(
    caller: Caller<'_, WasmState>,
    event: u32,
    data: u32,
    capacity: u32,
) -> i64 {
    call(caller, |state, memory| {
        let header = memory.record(event, layout_event::LAYOUT, false, None)?;
        let output = memory.admit(data, capacity, 1, 1, contract::MAX_EVENT_BYTES as u32)?;
        if header.overlaps(output) {
            return Err(HostError::InvalidArgument);
        }
        header.clear(memory.data);
        #[cfg(all(feature = "native-audio", target_vendor = "apple"))]
        crate::runtime::syscall::audio::pump(state);
        let count = events::peek_event(state)?.data.len();
        if count > capacity as usize {
            return Ok(count as i64);
        }
        let event = events::take_event(state)?;
        output.write(memory.data, 0, &event.data);
        header.write(
            memory.data,
            layout_event::GENERATION,
            &u64::from(state.generation).to_le_bytes(),
        );
        header.write(memory.data, layout_event::KIND, &(event.kind as u32).to_le_bytes());
        header.write(memory.data, layout_event::TARGET, &event.target.to_le_bytes());
        header.publish(memory.data, layout_event::LAYOUT);
        Ok(count as i64)
    })
}
pub(super) fn identity(caller: Caller<'_, WasmState>, pointer: u32) -> i64 {
    call(caller, |state, memory| {
        let output = memory.record(pointer, layout_identity::LAYOUT, false, None)?;
        output.clear(memory.data);
        let (generation, identity) = environment::identity(state)?;
        output.write(memory.data, layout_identity::GENERATION, &generation.to_le_bytes());
        output.write(memory.data, layout_identity::ACCOUNT_ID, &identity.account_id);
        output.write(memory.data, layout_identity::DEVICE_ID, &identity.device_id);
        output.write(memory.data, layout_identity::INSTANCE_ID, &identity.instance_id);
        output.write(
            memory.data,
            layout_identity::SESSION_ID,
            &identity.session_id.unwrap_or([0; 32]),
        );
        let flags =
            u32::from(identity.is_service) | (u32::from(identity.session_id.is_some()) << 1);
        output.write(memory.data, layout_identity::FLAGS, &flags.to_le_bytes());
        output.write(memory.data, layout_identity::RESERVED, &0_u32.to_le_bytes());
        output.publish(memory.data, layout_identity::LAYOUT);
        Ok(0)
    })
}
pub(super) fn wake_after(caller: Caller<'_, WasmState>, delay: u32) -> i64 {
    call(caller, |state, _| {
        control::wake_after(state, delay, std::time::Instant::now()).map(|()| 0)
    })
}
pub(super) fn quit_on_last_window(caller: Caller<'_, WasmState>, intent: u32) -> i64 {
    call(caller, |state, _| control::quit_on_last_window(state, intent).map(|()| 0))
}
pub(super) fn update_defer(caller: Caller<'_, WasmState>, generation: u64) -> i64 {
    call(caller, |state, _| control::update_defer(state, generation).map(|()| 0))
}

pub(super) fn trace_record(
    caller: Caller<'_, WasmState>,
    pointer: u32,
    length: u32,
    generation: u64,
    lost: u64,
) -> i64 {
    call(caller, |state, memory| {
        let input =
            memory.admit(pointer, length, 1, 1, playbit_api::abi::trace::MAX_BATCH_BYTES as u32)?;
        crate::runtime::syscall::tracing::record(state, input.bytes(memory.data), generation, lost)
            .map(|()| 0)
    })
}
