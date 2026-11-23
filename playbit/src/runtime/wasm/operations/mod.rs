//! Two scalar dispatch imports backed by checked shared operations and bounded memory.
use super::guest::{StoreData, WasmState};
use crate::runtime::{
    instance::State,
    linear_memory::Memory,
    syscall::{HostError, HostResult},
};
use wasmtime::{Caller, Extern, Linker};
mod audio;
mod authenticated;
#[cfg(test)]
mod authenticated_tests;
mod bloblog;
#[cfg(test)]
mod bloblog_tests;
pub(super) mod control;
#[cfg(test)]
mod entry_tests;
mod environment;
mod files;
#[cfg(test)]
mod frozen_guest_tests;
mod kv;
#[cfg(test)]
mod kv_tests;
use crate::runtime::linear_memory::output;
mod async_data;
mod pending;
#[cfg(test)]
mod pending_tests;
mod query;
#[cfg(test)]
mod query_tests;
mod read;
#[cfg(test)]
mod result_tests;
mod schema;
#[cfg(test)]
mod schema_tests;
#[cfg(test)]
mod tests;
mod volumes;

const HOST_CALL_FUEL: u64 = 1000;

pub(super) fn define(linker: &mut Linker<WasmState>) -> wasmtime::Result<()> {
    linker.func_wrap(
        "playbit",
        "pb_syscall",
        |caller: Caller<'_, WasmState>,
         operation: u32,
         a: u64,
         b: u64,
         c: u64,
         d: u64,
         e: u64,
         f: u64| { dispatch(caller, operation, [a, b, c, d, e, f]) },
    )?;
    Ok(())
}
pub(super) fn dispatch(
    mut caller: Caller<'_, impl StoreData>,
    operation: u32,
    arguments: [u64; 6],
) -> i64 {
    use playbit_api::abi::{dispatch, syscall::Opcode};
    let opcode = match dispatch::admit(operation, &arguments, 32) {
        Ok(opcode) => opcode,
        Err(error) => return error as i64,
    };
    if opcode == Opcode::ClockRead {
        if caller.data().fuel_limit().is_some() {
            let fuel = caller.get_fuel().expect("metered guest enables fuel");
            caller
                .set_fuel(fuel.saturating_sub(HOST_CALL_FUEL))
                .expect("fuel remains enabled throughout the synchronous import");
        }
        return crate::trace::clock_ns() as i64;
    }
    dispatch_operation(caller, opcode, arguments)
}

fn dispatch_operation(
    caller: Caller<'_, impl StoreData>,
    opcode: playbit_api::abi::syscall::Opcode,
    arguments: [u64; 6],
) -> i64 {
    use playbit_api::abi::syscall::Opcode;
    let [a, b, c, d, e, _f] = arguments;
    let (generation, slot) = playbit_api::abi::dispatch::unpack_handle(a);
    match opcode {
        operation if playbit_api::abi::audio::OPCODES.contains(&operation) => {
            audio::dispatch(caller, operation, arguments)
        }
        Opcode::ObjectReadBegin => {
            async_data::read_begin(caller, a as u32, b as u32, c as u32, d as u32)
        }
        Opcode::ObjectEditBegin => {
            async_data::edit_begin(caller, a as u32, b as u32, c as u32, d as u32, e as u32)
        }
        Opcode::ObjectQueryBegin => async_data::query_begin(caller, a as u32, b as u32),
        Opcode::ObjectReadTake => async_data::read_take(caller, generation, slot, b as u32),
        Opcode::ObjectEditTake => async_data::edit_take(caller, generation, slot),
        Opcode::ObjectQueryTake => async_data::query_take(caller, generation, slot, b as u32),
        Opcode::ObjectCreateBegin => {
            pending::create_begin(caller, a as u32, b as u32, c as u32, d as u32, e as u32)
        }
        Opcode::ObjectCreatePoll => pending::create_poll(caller, generation, slot, b as u32),
        Opcode::FileCopyBegin => pending::copy_begin(caller, a as u32, b as u32),
        Opcode::FileCopyPoll => pending::copy_poll(caller, generation, slot, b as u32),
        Opcode::FileSelectBegin => pending::select_begin(caller),
        Opcode::FileSelectPoll => pending::select_poll(caller, generation, slot, b as u32),
        Opcode::FileOpenBegin
        | Opcode::FileOpenTake
        | Opcode::FileReadBegin
        | Opcode::FileReadTake
        | Opcode::FileWriteBegin
        | Opcode::FileWriteTake
        | Opcode::FileStatusBegin
        | Opcode::FileStatusTake
        | Opcode::FileCloseBegin
        | Opcode::FileCloseTake
        | Opcode::FileListOpenBegin
        | Opcode::FileListOpenTake
        | Opcode::FileListNextBegin
        | Opcode::FileListNextTake
        | Opcode::FileListClose
        | Opcode::FileDispose
        | Opcode::OperationDispose => files::dispatch(caller, opcode, arguments),
        Opcode::SchemaReadBegin
        | Opcode::SchemaReadTake
        | Opcode::SchemaEstablishBegin
        | Opcode::SchemaEstablishTake
        | Opcode::EntryResolveBegin
        | Opcode::EntryResolveTake
        | Opcode::AvailableVolumes
        | Opcode::VolumesObserve => volume_dispatch(caller, opcode, arguments),
        Opcode::ObjectFieldReadAuthenticatedBegin => {
            authenticated::begin(caller, a as u32, b as u32, c as u32)
        }
        Opcode::ObjectFieldReadAuthenticatedTake => {
            authenticated::take(caller, generation, slot, b as u32)
        }
        _ => storage_dispatch(caller, opcode, arguments),
    }
}

fn volume_dispatch(
    caller: Caller<'_, impl StoreData>,
    opcode: playbit_api::abi::syscall::Opcode,
    arguments: [u64; 6],
) -> i64 {
    use playbit_api::abi::syscall::Opcode;
    let [a, b, c, d, _e, _] = arguments;
    let (generation, slot) = playbit_api::abi::dispatch::unpack_handle(a);
    match opcode {
        Opcode::SchemaReadBegin => schema::read_begin(caller, a as u32),
        Opcode::SchemaReadTake => schema::read_take(caller, generation, slot, b as u32),
        Opcode::SchemaEstablishBegin => {
            schema::establish_begin(caller, a as u32, b as u32, c as u32, d as u32)
        }
        Opcode::SchemaEstablishTake => schema::establish_take(caller, generation, slot),
        Opcode::EntryResolveBegin => {
            volumes::resolve_begin(caller, generation, slot, b as u32, c as u32)
        }
        Opcode::EntryResolveTake => volumes::resolve_take(caller, generation, slot, b as u32),
        Opcode::AvailableVolumes => volumes::volumes(caller, a as u32),
        Opcode::VolumesObserve => call(caller, |state, _| {
            crate::runtime::syscall::control::observe_volumes(state, a).map(|()| 0)
        }),
        _ => unreachable!("volume/schema opcode group"),
    }
}

fn platform_dispatch(
    caller: Caller<'_, impl StoreData>,
    opcode: playbit_api::abi::syscall::Opcode,
    arguments: [u64; 6],
) -> i64 {
    use playbit_api::abi::syscall::Opcode;
    let [a, b, c, d, _, _] = arguments;
    match opcode {
        Opcode::Session | Opcode::ChannelOpen | Opcode::ChannelBegin | Opcode::ChannelTake => {
            channel_call(caller, opcode, arguments)
        }

        Opcode::HostPlatform => environment::host_platform(caller),
        Opcode::HostLimit => environment::host_limit(caller, a as u32),
        Opcode::RandomFill => environment::random_fill(caller, a as u32, b as u32),
        Opcode::TraceRecord => control::trace_record(caller, a as u32, b as u32, c, d),
        Opcode::Capabilities => environment::capabilities(caller, a as u32),
        Opcode::StartupArguments => environment::startup_arguments(caller, a as u32),
        Opcode::Presence => environment::presence(caller, a as u32, b as u32, c as u32, d as u32),

        Opcode::Log => control::log(caller, a as u32, b as u32),
        Opcode::ViewSubmit => control::view_submit(caller, a as u32, b as u32),
        Opcode::EventPollDeadline => HostError::Unsupported as i64,
        Opcode::EventPoll => control::event_poll(caller, a as u32, b as u32, c as u32),
        Opcode::Identity => control::identity(caller, a as u32),
        Opcode::SessionInfo => control::session_info(caller, a as u32),
        Opcode::WakeAfter => control::wake_after(caller, a as u32),
        Opcode::QuitOnLastWindow => control::quit_on_last_window(caller, a as u32),
        Opcode::UpdateDefer => control::update_defer(caller, a),
        _ => unreachable!("platform opcode group"),
    }
}

fn call<T: Into<i64>>(
    mut caller: Caller<'_, impl StoreData>,
    operation: impl FnOnce(&mut State, &mut Memory<'_>) -> HostResult<T>,
) -> i64 {
    call_borrowed(&mut caller, operation)
}

fn call_borrowed<T: Into<i64>>(
    caller: &mut Caller<'_, impl StoreData>,
    operation: impl FnOnce(&mut State, &mut Memory<'_>) -> HostResult<T>,
) -> i64 {
    let Some(Extern::Memory(memory)) = caller.get_export("memory") else {
        return HostError::InvalidArgument as i64;
    };
    let fuel = if caller.data().fuel_limit().is_some() {
        let fuel = caller.get_fuel().expect("metered guest enables fuel");
        if fuel <= HOST_CALL_FUEL {
            caller.set_fuel(0).expect("metered guest enables fuel");
            return HostError::LimitExceeded as i64;
        }
        Some(fuel - HOST_CALL_FUEL)
    } else {
        None
    };
    // compile_app rejects shared memory; this linker exposes no imported memory.
    // While the synchronous import owns the caller, guest execution is suspended.
    let (data, state) = memory.data_and_store_mut(&mut *caller);
    let mut memory = Memory { data, fuel };
    let result = state.with_app(|app| {
        app.assert_invariants();
        let result =
            if app.halted { Err(HostError::StaleHandle) } else { operation(app, &mut memory) };
        app.assert_invariants();
        result
    });
    let remaining = memory.fuel;
    if let Some(remaining) = remaining {
        caller.set_fuel(remaining).expect("fuel remains enabled throughout the synchronous import");
    }
    result.map(Into::into).unwrap_or_else(|error| error as i64)
}

fn storage_dispatch(
    caller: Caller<'_, impl StoreData>,
    opcode: playbit_api::abi::syscall::Opcode,
    arguments: [u64; 6],
) -> i64 {
    use playbit_api::abi::syscall::Opcode;
    let [a, b, c, d, e, f] = arguments;
    let (generation, slot) = playbit_api::abi::dispatch::unpack_handle(a);
    match opcode {
        Opcode::BloblogAppendBegin => {
            bloblog::append_begin(caller, a, b as u32, c as u32, d as u32, e as u32, f as u32)
        }
        Opcode::BloblogReadBegin => bloblog::read_begin(caller, a, b as u32, c as u32, d as u32),
        Opcode::BloblogAppendTake | Opcode::BloblogReadTake => {
            bloblog::take(caller, generation, slot, opcode, b as u32, c as u32, d as u32)
        }
        Opcode::KvKeysBegin => kv::keys_begin(caller, a as u32, b as u32, c as u32, d as u32),
        Opcode::KvReadBegin => kv::read_begin(caller, a as u32, b as u32, c as u32, d as u32),
        Opcode::KvPutBegin => {
            kv::put_begin(caller, a as u32, b as u32, c as u32, d as u32, e as u32)
        }
        Opcode::KvDeleteBegin => kv::delete_begin(caller, a as u32, b as u32, c as u32),
        Opcode::KvKeysTake | Opcode::KvReadTake => {
            kv::bytes_take(caller, generation, slot, opcode, b as u32, c as u32, d as u32)
        }
        Opcode::KvPutTake | Opcode::KvDeleteTake => {
            kv::receipt_take(caller, generation, slot, opcode)
        }
        _ => platform_dispatch(caller, opcode, arguments),
    }
}

fn channel_call(
    caller: Caller<'_, impl StoreData>,
    opcode: playbit_api::abi::syscall::Opcode,
    slots: [u64; 6],
) -> i64 {
    use crate::runtime::syscall::{HostError, Opcode, OperationHandle, channels};
    let [a, b, c, d, _e, _] = slots;
    call(caller, |state, memory| match opcode {
        Opcode::Session => Ok(i64::from(channels::session(state)?)),
        Opcode::ChannelOpen => {
            let name = memory.input(a as u32, b as u32, 1, 1, 223, None)?;
            let name = std::str::from_utf8(name.bytes(memory.data))
                .map_err(|_| HostError::InvalidArgument)?;
            let handle = channels::open(state, name)?;
            Ok(playbit_api::abi::dispatch::pack_handle(handle.generation, handle.slot) as i64)
        }
        Opcode::ChannelBegin => {
            let data = memory
                .input(c as u32, d as u32, 1, 1, 32 * 1024, None)?
                .bytes(memory.data)
                .to_vec();
            let (generation, slot) = playbit_api::abi::dispatch::unpack_handle(a);
            let handle =
                channels::begin(state, OperationHandle { generation, slot }, b as u32, data)?;
            Ok(playbit_api::abi::dispatch::pack_handle(handle.generation, handle.slot) as i64)
        }
        Opcode::ChannelTake => {
            let (generation, slot) = playbit_api::abi::dispatch::unpack_handle(a);
            channels::take(state, OperationHandle { generation, slot }).map(|_| 0)
        }
        _ => Err(HostError::Unsupported),
    })
}
