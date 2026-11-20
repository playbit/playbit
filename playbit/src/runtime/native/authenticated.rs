//! Authenticated field output is copied from one shared authorized snapshot.
use super::{
    calls::{span, with_context},
    data::{append_bytes, entry_handle, output_span, output_value},
};
use crate::runtime::syscall::{self as rt, HostError, pending_data};
use playbit_api::abi::{
    authenticated as abi, data, operations::Context, pending, syscall as contract,
};
fn metadata(record: &rt::AuthenticatedFieldRead) -> abi::Metadata {
    let mut metadata = abi::Metadata {
        volume_id: record.volume_id,
        entry_id: record.entry_id,
        object_id: record.object_id,
        type_id: record.type_id,
        field_id: record.field_id,
        revision: record.revision.0,
        state: if record.values.is_some() { 2 } else { 1 },
        ..abi::Metadata::default()
    };
    let (provenance, principal, device) = match record.provenance {
        rt::FieldProvenance::Unproven => return metadata,
        rt::FieldProvenance::ApplicationEdit { principal, device_id } => (1, principal, device_id),
        rt::FieldProvenance::Rekey { principal, device_id } => (2, principal, device_id),
    };
    let (kind, id) = match principal {
        rt::Principal::Human(id) => (1, id),
        rt::Principal::Service(id) => (2, id),
    };
    metadata.provenance = provenance;
    metadata.principal_kind = kind;
    metadata.principal_id = id;
    metadata.device_id = device;
    metadata
}
/// # Safety
/// Stable ID inputs obey playbit/sys/abi.h.
pub(super) unsafe extern "C" fn begin(
    context: *mut Context,
    entry: *const data::EntryHandle,
    object: *const [u8; 16],
    field: *const [u8; 16],
) -> i64 {
    // SAFETY: every region is admitted before starting accepted host work.
    unsafe {
        with_context(context, |state| {
            let entry = entry_handle(entry)?;
            pending_data::admit_entry(state, entry, rt::Opcode::ObjectFieldReadAuthenticatedBegin)?;
            span(object, 1)?;
            span(field, 1)?;
            let operation = pending_data::begin_entry(
                state,
                entry,
                crate::runtime::host::DataOperation::AuthenticatedField {
                    entry_id: entry.entry_id,
                    object_id: object.read(),
                    field_id: field.read(),
                },
                rt::Opcode::ObjectFieldReadAuthenticatedBegin,
            )?;
            Ok((pending::OperationHandle::new(operation.generation, operation.slot)).0 as i64)
        })
    }
}
/// # Safety
/// Initialized descriptor and disjoint arrays obey playbit/sys/abi.h.
unsafe fn write_result(
    record: Option<&rt::AuthenticatedFieldRead>,
    output: *mut abi::Field,
    mut result: abi::Field,
) -> crate::runtime::host::HostResult<i32> {
    // SAFETY: caller admitted every independent output before consuming the result.
    unsafe {
        let values = record.and_then(|r| r.values.as_deref()).unwrap_or_default();
        result.values_count = values.len() as u32;
        result.bytes_count =
            values.iter().filter_map(|v| output_value(v).1).map(|b| b.len() as u32).sum();
        if result.values_count > result.values_capacity
            || result.bytes_count > result.bytes_capacity
        {
            super::record::counts(output, result)?;
            return Err(HostError::BufferTooSmall);
        }
        let mut used = 0;
        for (index, value) in values.iter().enumerate() {
            let (mut value, bytes) = output_value(value);
            if let Some(bytes) = bytes {
                value.set_bytes(append_bytes(result.bytes_data, &mut used, bytes));
            }
            super::record::element(result.values_data, result.values_stride, index, value)?;
        }
        assert_eq!(used, result.bytes_count);
        super::record::write(result.metadata, record.map(metadata).unwrap_or_default())?;
        super::record::write(output, result)?;
        Ok(0)
    }
}

/// # Safety
/// Initialized descriptor and disjoint arrays obey playbit/sys/abi.h.
pub(super) unsafe extern "C" fn take(
    context: *mut Context,
    operation: pending::OperationHandle,
    output: *mut abi::Field,
) -> i64 {
    // SAFETY: admit all output before inspecting or consuming retained memory.
    unsafe {
        with_context(context, |state| {
            let operation =
                rt::OperationHandle { generation: operation.generation(), slot: operation.slot() };
            pending_data::admit_take(
                state,
                operation,
                rt::Opcode::ObjectFieldReadAuthenticatedTake,
            )?;
            let descriptor = super::record::region(output, true)?;
            let result = super::record::read(output, false)?;
            super::record::outputs(&[
                descriptor,
                super::record::region(result.metadata, true)?,
                output_span(&result.values(), contract::MAX_VALUES as usize)?,
                output_span(&result.bytes(), contract::MAX_DATA_BYTES)?,
            ])?;
            playbit_api::abi::record::clear_native(output)?;
            playbit_api::abi::record::clear_native(result.metadata)?;
            super::record::clear_array(&result.values());
            pending_data::take(
                state,
                operation,
                rt::Opcode::ObjectFieldReadAuthenticatedTake,
                |_, record| {
                    let crate::runtime::host::DataResult::AuthenticatedField(record) = record
                    else {
                        return Err(HostError::Internal);
                    };
                    write_result(record.as_ref(), output, result)
                },
            )
        })
    }
}
