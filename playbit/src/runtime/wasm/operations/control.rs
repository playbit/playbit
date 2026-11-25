use super::{StoreData, call};
use crate::runtime::linear_memory::layout::operations::{
    event as layout_event, identity as layout_identity,
};
use crate::runtime::syscall::{HostError, control, environment, events};
use playbit_api::abi::syscall as contract;
use wasmtime::Caller;

pub(super) fn log(caller: Caller<'_, impl StoreData>, pointer: u32, length: u32) -> i64 {
    call(caller, |state, memory| {
        let input = memory.admit(pointer, length, 1, 1, u32::MAX)?;
        let text = std::str::from_utf8(input.bytes(memory.data))
            .map_err(|_| HostError::InvalidArgument)?;
        control::log(state, text).map(|()| 0)
    })
}
pub(super) fn view_submit(caller: Caller<'_, impl StoreData>, pointer: u32, length: u32) -> i64 {
    call(caller, |state, memory| {
        let input = memory.admit(pointer, length, 1, 1, state.view_bytes_limit)?;
        control::view_submit(state, input.bytes(memory.data)).map(|()| 0)
    })
}
pub(super) fn event_poll(
    caller: Caller<'_, impl StoreData>,
    event: u32,
    data: u32,
    capacity: u32,
) -> i64 {
    call(caller, |state, memory| {
        let regions = event_regions(memory, event, data, capacity)?;
        #[cfg(all(feature = "native-audio", target_vendor = "apple"))]
        crate::runtime::syscall::audio::pump(state);
        write_event(state, memory, regions)
    })
}

use crate::runtime::{
    instance::State,
    linear_memory::{Memory, Region},
    syscall::HostResult,
};

fn event_regions(
    memory: &mut Memory<'_>,
    event: u32,
    data: u32,
    capacity: u32,
) -> HostResult<(Region, Region)> {
    let header = memory.record(event, layout_event::OUTPUT, false, None)?;
    let output = memory.admit(data, capacity, 1, 1, contract::MAX_EVENT_BYTES as u32)?;
    if header.overlaps(output) {
        return Err(HostError::InvalidArgument);
    }
    header.clear(memory.data);
    Ok((header, output))
}

fn write_event(
    state: &mut State,
    memory: &mut Memory<'_>,
    (header, output): (Region, Region),
) -> HostResult<i64> {
    let count = events::peek_event(state)?.data.len();
    if count > output.len() {
        return Ok(count as i64);
    }
    let event = events::take_event(state)?;
    output.write(memory.data, 0, &event.data);
    header.write(memory.data, layout_event::GENERATION, &u64::from(state.generation).to_le_bytes());
    header.write(memory.data, layout_event::KIND, &(event.kind as u32).to_le_bytes());
    header.write(memory.data, layout_event::TARGET, &event.target.to_le_bytes());
    if header.len() >= layout_event::OBJECT_ID + 4 {
        header.write(memory.data, layout_event::OBJECT_ID, &event.object_id.to_le_bytes());
    }
    header.publish(memory.data, layout_event::LAYOUT);
    Ok(count as i64)
}

pub(in crate::runtime::wasm) async fn deadline(
    mut caller: Caller<'_, super::super::deadline::DeadlineData>,
    event: u32,
    data: u32,
    capacity: u32,
    deadline: u64,
) -> i64 {
    // Re-admit the output on every resume. Only the scalar offsets survive the
    // yield; the caller-size field is preserved when clearing output metadata.
    loop {
        let mut waiting = false;
        let result = super::call_borrowed(&mut caller, |state, memory| {
            let regions = event_regions(memory, event, data, capacity)?;
            waiting = state.arm_poll_deadline(deadline)?;
            if waiting { Ok(0) } else { write_event(state, memory, regions) }
        });
        if !waiting {
            return result;
        }
        let mut first = true;
        std::future::poll_fn(|_| {
            if std::mem::take(&mut first)
                || caller.data_mut().with_app(|state| state.arm_poll_deadline(deadline)) == Ok(true)
            {
                std::task::Poll::Pending
            } else {
                std::task::Poll::Ready(())
            }
        })
        .await;
        // Only an event, timeout or lifecycle change can resume the caller.
        // Spurious owner pumps leave both its stack and fuel budget parked.
        if let Some(limit) = caller.data().fuel_limit() {
            caller.set_fuel(limit.get()).expect("metered guest enables fuel");
        }
    }
}
pub(super) fn identity(caller: Caller<'_, impl StoreData>, pointer: u32) -> i64 {
    call(caller, |state, memory| {
        let output = memory.record(pointer, layout_identity::OUTPUT, false, None)?;
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
        if output.len()
            >= layout_identity::DISPLAY_NAME + playbit_api::abi::environment::DISPLAY_NAME_BYTES
        {
            let name = environment::display_name_field(&identity.display_name);
            output.write(memory.data, layout_identity::DISPLAY_NAME, &name);
        }
        output.publish(memory.data, layout_identity::LAYOUT);
        Ok(0)
    })
}
pub(super) fn session_info(caller: Caller<'_, impl StoreData>, pointer: u32) -> i64 {
    use crate::runtime::linear_memory::layout::session::session_info as layout;
    call(caller, |state, memory| {
        let output = memory.record(pointer, layout::LAYOUT, false, None)?;
        output.clear(memory.data);
        let info = environment::session_info(state)?;
        output.write(memory.data, layout::FLAGS, &info.flags.to_le_bytes());
        output.write(memory.data, layout::RESERVED, &info.reserved.to_le_bytes());
        output.write(memory.data, layout::INTERNAL_ID, &info.internal_id);
        output.write(memory.data, layout::KEY_SECRET, &info.key_secret);
        output.publish(memory.data, layout::LAYOUT);
        Ok(0)
    })
}
pub(super) fn wake_after(caller: Caller<'_, impl StoreData>, delay: u32) -> i64 {
    call(caller, |state, _| {
        control::wake_after(state, delay, std::time::Instant::now()).map(|()| 0)
    })
}
pub(super) fn quit_on_last_window(caller: Caller<'_, impl StoreData>, intent: u32) -> i64 {
    call(caller, |state, _| control::quit_on_last_window(state, intent).map(|()| 0))
}
pub(super) fn update_defer(caller: Caller<'_, impl StoreData>, generation: u64) -> i64 {
    call(caller, |state, _| control::update_defer(state, generation).map(|()| 0))
}

pub(super) fn trace_record(
    caller: Caller<'_, impl StoreData>,
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
