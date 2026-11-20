use super::{WasmState, call, output::Buffer};
use crate::runtime::linear_memory::layout::{
    data::range,
    environment::{
        argument, arguments as args_layout, capabilities as caps_layout,
        participant as participant_layout,
    },
};
use crate::runtime::syscall::{HostError, environment};
use playbit_api::abi::syscall as contract;
use wasmtime::Caller;

pub(super) fn random_fill(caller: Caller<'_, WasmState>, pointer: u32, length: u32) -> i64 {
    call(caller, |state, memory| {
        let output = memory.admit(pointer, length, 1, 1, contract::MAX_RANDOM_BYTES as u32)?;
        environment::random_fill(state, output.bytes_mut(memory.data)).map(|()| 0)
    })
}

pub(super) fn capabilities(caller: Caller<'_, WasmState>, pointer: u32) -> i64 {
    call(caller, |state, memory| {
        let descriptor = memory.record(pointer, caps_layout::LAYOUT, false, None)?;
        let names = Buffer::records(
            memory,
            descriptor.sub(caps_layout::NAMES_DATA, 16),
            range::LAYOUT,
            256,
        )?;
        let bytes =
            Buffer::admit(memory, descriptor.sub(caps_layout::BYTES_DATA, 12), 1, 1, 32 * 1024)?;
        crate::runtime::linear_memory::record::outputs(&[descriptor, names.data, bytes.data])?;
        descriptor.clear(memory.data);
        names.clear(memory.data);
        let capabilities = environment::capabilities(state)?;
        let size = capabilities.iter().map(|op| op.len()).sum();
        names.count(memory.data, capabilities.len());
        bytes.count(memory.data, size);
        if !names.fits(capabilities.len()) || !bytes.fits(size) {
            return Err(HostError::BufferTooSmall);
        }
        let mut used = 0;
        for (index, operation) in capabilities.iter().enumerate() {
            let name = bytes.append(memory.data, &mut used, operation.as_bytes());
            names.element(memory.data, index).write(memory.data, range::OFFSET, &name);
            names.publish(memory.data, index);
        }
        assert_eq!(used, size);
        descriptor.publish(memory.data, caps_layout::LAYOUT);
        Ok(0)
    })
}

pub(super) fn startup_arguments(caller: Caller<'_, WasmState>, pointer: u32) -> i64 {
    call(caller, |state, memory| {
        let descriptor = memory.record(pointer, args_layout::LAYOUT, false, None)?;
        let arguments = Buffer::records(
            memory,
            descriptor.sub(args_layout::ARGUMENTS_DATA, 16),
            argument::LAYOUT,
            contract::MAX_STARTUP_ARGUMENTS as u32,
        )?;
        let bytes =
            Buffer::admit(memory, descriptor.sub(args_layout::BYTES_DATA, 12), 1, 1, 32 * 1024)?;
        crate::runtime::linear_memory::record::outputs(&[descriptor, arguments.data, bytes.data])?;
        descriptor.clear(memory.data);
        arguments.clear(memory.data);
        let values = environment::startup_arguments(state)?;
        let size = values.iter().map(|(key, value)| key.len() + value.len()).sum();
        arguments.count(memory.data, values.len());
        bytes.count(memory.data, size);
        if !arguments.fits(values.len()) || !bytes.fits(size) {
            return Err(HostError::BufferTooSmall);
        }
        let mut used = 0;
        for (index, (key, value)) in values.iter().enumerate() {
            let key = bytes.append(memory.data, &mut used, key.as_bytes());
            let value = bytes.append(memory.data, &mut used, value.as_bytes());
            let record = arguments.element(memory.data, index);
            record.write(memory.data, argument::NAME_OFFSET, &key);
            record.write(memory.data, argument::VALUE_OFFSET, &value);
            arguments.publish(memory.data, index);
        }
        assert_eq!(used, size);
        descriptor.publish(memory.data, args_layout::LAYOUT);
        Ok(0)
    })
}

pub(super) fn presence(
    caller: Caller<'_, WasmState>,
    pointer: u32,
    capacity: u32,
    stride: u32,
    count: u32,
) -> i64 {
    call(caller, |state, memory| {
        let count = memory.admit(count, 1, 4, 4, 1)?;
        let output = memory.record_array(
            pointer,
            capacity,
            stride,
            participant_layout::LAYOUT,
            false,
            1024,
            Some(count),
        )?;
        for index in 0..capacity as usize {
            output.slot(memory.data, index, stride).clear(memory.data);
        }
        let participants = environment::presence(state)?;
        count.write(memory.data, 0, &(participants.len() as u32).to_le_bytes());
        if participants.len() > capacity as usize {
            return Err(HostError::BufferTooSmall);
        }
        for (index, participant) in participants.iter().enumerate() {
            let record = output.slot(memory.data, index, stride);
            record.write(memory.data, participant_layout::ACCOUNT_ID, &participant.account_id);
            record.write(memory.data, participant_layout::DEVICE_ID, &participant.device_id);
            record.write(memory.data, participant_layout::INSTANCE_ID, &participant.instance_id);
            record.write(memory.data, participant_layout::BUILD_ID, &participant.build_id);
            record.publish(memory.data, participant_layout::LAYOUT);
        }
        Ok(0)
    })
}

pub(super) fn host_platform(caller: Caller<'_, WasmState>) -> i64 {
    call(caller, |state, _| environment::host_platform(state))
}

pub(super) fn host_limit(caller: Caller<'_, WasmState>, kind: u32) -> i64 {
    call(caller, |state, _| environment::host_limit(state, kind))
}
