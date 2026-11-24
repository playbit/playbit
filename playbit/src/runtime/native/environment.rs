//! Native metadata copies from the checked instance into caller-owned arrays.
//! No borrowed host pointer or scratch allocation survives a call.
use super::{
    calls::{span, with_context},
    data::{append_bytes, disjoint, output_span},
};
use crate::runtime::syscall::{HostError, environment};
use playbit_api::abi::{environment as abi, operations::Context, syscall as contract};
const MAX_CAPABILITIES: usize = 256;
const MAX_METADATA_BYTES: usize = 32 * 1024;
const MAX_PARTICIPANTS: usize = 1024;
const _: () = assert!(contract::REGISTRY.len() <= MAX_CAPABILITIES);
const _: () = assert!(contract::MAX_STARTUP_ARGUMENT_JSON_BYTES <= MAX_METADATA_BYTES);

/// # Safety
/// Context and writable output obey the callback borrowing contract.
pub(super) unsafe fn pb_random_fill(context: *mut Context, bytes: *mut u8, length: u32) -> i64 {
    // SAFETY: structural admission precedes initialization and borrowing. Native
    // callers guarantee allocation validity; no reference escapes this call.
    unsafe {
        with_context(context, |state| {
            if length as usize > contract::MAX_RANDOM_BYTES {
                return Err(HostError::LimitExceeded);
            }
            span(bytes, length as usize)?;
            if length == 0 {
                return environment::random_fill(state, &mut []).map(|()| 0);
            }
            // Output need not be initialized by a C caller. Initialize before
            // constructing a Rust slice, including when the entropy source fails.
            std::ptr::write_bytes(bytes, 0, length as usize);
            let output = std::slice::from_raw_parts_mut(bytes, length as usize);
            environment::random_fill(state, output).map(|()| 0)
        })
    }
}

/// # Safety
/// Initialized descriptor and disjoint arrays obey playbit/sys/abi.h.
pub(super) unsafe extern "C" fn pb_capabilities(
    context: *mut Context,
    output: *mut abi::Capabilities,
) -> i64 {
    // SAFETY: all output regions/capacities are admitted before the first write.
    unsafe {
        with_context(context, |state| {
            let descriptor = super::record::region(output, true)?;
            let mut result = super::record::read(output, false)?;
            super::record::outputs(&[
                descriptor,
                output_span(&result.names(), MAX_CAPABILITIES)?,
                output_span(&result.bytes(), MAX_METADATA_BYTES)?,
            ])?;
            playbit_api::abi::record::clear_native(output)?;
            super::record::clear_array(&result.names());
            let names = environment::capabilities(state)?;
            result.names_count = names.len() as u32;
            result.bytes_count = names.iter().map(|op| op.len() as u32).sum();
            assert!(result.bytes_count as usize <= MAX_METADATA_BYTES);
            if result.names_count > result.names_capacity
                || result.bytes_count > result.bytes_capacity
            {
                super::record::counts(output, result)?;
                return Err(HostError::BufferTooSmall);
            }
            let mut used = 0;
            for (index, op) in names.iter().enumerate() {
                let name = append_bytes(result.bytes_data, &mut used, op.as_bytes());
                super::record::element(result.names_data, result.names_stride, index, name)?;
            }
            assert_eq!(used, result.bytes_count);
            super::record::write(output, result)?;
            Ok(0)
        })
    }
}
/// # Safety
/// Initialized descriptor and disjoint arrays obey playbit/sys/abi.h.
pub(super) unsafe extern "C" fn pb_startup_arguments(
    context: *mut Context,
    output: *mut abi::Arguments,
) -> i64 {
    // SAFETY: the validated instance bounds all strings; writes follow complete admission.
    unsafe {
        with_context(context, |state| {
            let descriptor = super::record::region(output, true)?;
            let mut result = super::record::read(output, false)?;
            super::record::outputs(&[
                descriptor,
                output_span(&result.arguments(), contract::MAX_STARTUP_ARGUMENTS)?,
                output_span(&result.bytes(), MAX_METADATA_BYTES)?,
            ])?;
            playbit_api::abi::record::clear_native(output)?;
            super::record::clear_array(&result.arguments());
            let arguments = environment::startup_arguments(state)?;
            result.arguments_count = arguments.len() as u32;
            result.bytes_count = arguments.iter().map(|(k, v)| (k.len() + v.len()) as u32).sum();
            assert!(result.bytes_count as usize <= MAX_METADATA_BYTES);
            if result.arguments_count > result.arguments_capacity
                || result.bytes_count > result.bytes_capacity
            {
                super::record::counts(output, result)?;
                return Err(HostError::BufferTooSmall);
            }
            let mut used = 0;
            for (index, (name, value)) in arguments.iter().enumerate() {
                let name = append_bytes(result.bytes_data, &mut used, name.as_bytes());
                let value = append_bytes(result.bytes_data, &mut used, value.as_bytes());
                super::record::element(
                    result.arguments_data,
                    result.arguments_stride,
                    index,
                    abi::Argument {
                        size: std::mem::size_of::<abi::Argument>() as u32,
                        written: 0,
                        name_offset: name.offset,
                        name_count: name.count,
                        value_offset: value.offset,
                        value_count: value.count,
                    },
                )?;
            }
            assert_eq!(used, result.bytes_count);
            super::record::write(output, result)?;
            Ok(0)
        })
    }
}
/// # Safety
/// Initialized descriptor and disjoint participant storage obey playbit/sys/abi.h.
pub(super) unsafe extern "C" fn pb_presence(
    context: *mut Context,
    output: *mut abi::Participant,
    capacity: u32,
    stride: u32,
    count: *mut u32,
) -> i64 {
    // SAFETY: no result writes occur until descriptor and array are admitted.
    unsafe {
        with_context(context, |state| {
            let records = super::record::array(output, capacity, stride, MAX_PARTICIPANTS)?;
            disjoint(&[records, span(count, 1)?])?;
            super::record::clear_array(&playbit_api::abi::data::Buffer {
                data: output,
                capacity,
                count: 0,
                stride,
            });
            let participants = environment::presence(state)?;
            count.write(participants.len() as u32);
            if participants.len() > capacity as usize {
                return Err(HostError::BufferTooSmall);
            }
            for (index, participant) in participants.iter().enumerate() {
                super::record::element(
                    output,
                    stride,
                    index,
                    abi::Participant {
                        account_id: participant.account_id,
                        device_id: participant.device_id,
                        instance_id: participant.instance_id,
                        build_id: participant.build_id,
                        display_name: environment::display_name_field(&participant.display_name),
                        ..abi::Participant::default()
                    },
                )?;
            }
            Ok(0)
        })
    }
}
/// # Safety
/// Context must be a live callback borrow as specified in playbit/sys/abi.h.
pub(super) unsafe extern "C" fn pb_host_platform(context: *mut Context) -> i64 {
    // SAFETY: the context is borrowed only for this call, with no caller buffers.
    unsafe { with_context(context, |state| environment::host_platform(state)) }
}

/// # Safety
/// Context obeys the native call lifetime and owner-thread contract.
pub(super) unsafe extern "C" fn pb_host_limit(context: *mut Context, kind: u32) -> i64 {
    // SAFETY: context admission precedes reading the immutable instance limits.
    unsafe { with_context(context, |state| environment::host_limit(state, kind)) }
}
