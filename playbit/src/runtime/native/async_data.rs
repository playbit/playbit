//! Typed asynchronous object operations; no I/O or mutation replay in take.
use super::{
    calls::{span, with_context},
    data::{decode_changes, entry_handle, input, output_span},
};
use crate::runtime::{
    host::{DataOperation, DataResult},
    syscall::{self as rt, pending_data},
};
use playbit_api::abi::{
    data as abi, operations::Context, pending::OperationHandle, syscall as contract,
};
#[cfg(test)]
#[path = "async_data_tests.rs"]
mod tests;

fn operation(value: OperationHandle) -> rt::OperationHandle {
    rt::OperationHandle { generation: value.generation(), slot: value.slot() }
}

fn result(value: rt::OperationHandle) -> OperationHandle {
    OperationHandle::new(value.generation, value.slot)
}

/// # Safety
/// Stable object and field inputs obey the C contract.
pub(super) unsafe fn read_begin(
    context: *mut Context,
    entry: *const abi::EntryHandle,
    object: *const rt::Id,
    fields: *const rt::Id,
    count: u32,
) -> i64 {
    // SAFETY: admit every pointer before copying the owned input or admission.
    unsafe {
        with_context(context, |state| {
            let entry = entry_handle(entry)?;
            pending_data::admit_entry(state, entry, rt::Opcode::ObjectReadBegin)?;
            span(object, 1)?;
            span(fields, count as usize)?;
            let fields = input(fields, count as usize, contract::MAX_FIELDS as usize)?.to_vec();
            let pending = pending_data::begin_entry(
                state,
                entry,
                DataOperation::Read { entry_id: entry.entry_id, object_id: object.read(), fields },
                rt::Opcode::ObjectReadBegin,
            )?;
            Ok((result(pending)).0 as i64)
        })
    }
}

/// # Safety
/// All nested inputs obey the C contract.
pub(super) unsafe fn edit_begin(
    context: *mut Context,
    entry: *const abi::EntryHandle,
    object: *const rt::Id,
    fields: *const abi::FieldInput,
    count: u32,
    stride: u32,
) -> i64 {
    // SAFETY: nested input spans are admitted before copying and host work.
    unsafe {
        with_context(context, |state| {
            let entry = entry_handle(entry)?;
            pending_data::admit_entry(state, entry, rt::Opcode::ObjectEditBegin)?;
            span(object, 1)?;
            let fields = decode_changes(fields, count, stride)?;
            let pending = pending_data::begin_entry(
                state,
                entry,
                DataOperation::Edit { entry_id: entry.entry_id, object_id: object.read(), fields },
                rt::Opcode::ObjectEditBegin,
            )?;
            Ok((result(pending)).0 as i64)
        })
    }
}

/// # Safety
/// Stable query and projection inputs obey the C contract.
pub(super) unsafe fn query_begin(
    context: *mut Context,
    entry: *const abi::EntryHandle,
    query: *const abi::Query,
) -> i64 {
    // SAFETY: descriptors and projection are stable until their owned copy.
    unsafe {
        with_context(context, |state| {
            let entry = entry_handle(entry)?;
            pending_data::admit_entry(state, entry, rt::Opcode::ObjectQueryBegin)?;
            super::record::region(query, false)?;
            let query = super::record::read(query, true)?;
            if query.reserved != 0
                || query.flags & !3 != 0
                || query.flags & 1 == 0 && query.after != [0; 16]
                || query.flags & 2 == 0 && query.count != 0
            {
                return Err(rt::HostError::InvalidArgument);
            }
            span(query.fields, query.count as usize)?;
            let ids = input(query.fields, query.count as usize, contract::MAX_FIELDS as usize)?;
            let pending = pending_data::begin_entry(
                state,
                entry,
                DataOperation::Query {
                    entry_id: entry.entry_id,
                    type_id: query.type_id,
                    after: (query.flags & 1 != 0).then_some(query.after),
                    limit: query.limit,
                    projection: (query.flags & 2 != 0).then(|| ids.to_vec()),
                },
                rt::Opcode::ObjectQueryBegin,
            )?;
            Ok((result(pending)).0 as i64)
        })
    }
}

/// # Safety
/// All caller-owned result spans are valid and mutually disjoint.
pub(super) unsafe fn read_take(
    context: *mut Context,
    pending: OperationHandle,
    output: *mut abi::Object,
) -> i64 {
    // SAFETY: complete structural admission precedes peek and consumption.
    unsafe {
        with_context(context, |state| {
            pending_data::admit_take(state, operation(pending), rt::Opcode::ObjectReadTake)?;
            let descriptor = super::record::region(output, true)?;
            let result = super::record::read(output, false)?;
            super::record::outputs(&[
                descriptor,
                output_span(&result.fields(), contract::MAX_FIELDS as usize)?,
                output_span(&result.values(), contract::MAX_VALUES as usize)?,
                output_span(&result.bytes(), contract::MAX_DATA_BYTES)?,
            ])?;
            playbit_api::abi::record::clear_native(output)?;
            super::record::clear_array(&result.fields());
            super::record::clear_array(&result.values());
            pending_data::take(state, operation(pending), rt::Opcode::ObjectReadTake, |_, data| {
                let DataResult::Object(object) = data else { return Err(rt::HostError::Internal) };
                super::data::write_object(object, output, result)
            })
        })
    }
}

/// # Safety
/// Context obeys the native C contract; this call has no pointer output.
pub(super) unsafe fn edit_take(context: *mut Context, pending: OperationHandle) -> i64 {
    // SAFETY: only the admitted context is borrowed.
    unsafe {
        with_context(context, |state| {
            pending_data::admit_take(state, operation(pending), rt::Opcode::ObjectEditTake)?;
            pending_data::take(state, operation(pending), rt::Opcode::ObjectEditTake, |_, data| {
                if matches!(data, DataResult::Edited) {
                    Ok(0)
                } else {
                    Err(rt::HostError::Internal)
                }
            })
        })
    }
}

/// # Safety
/// Page descriptor and all caller-owned arrays are valid and mutually disjoint.
pub(super) unsafe fn query_take(
    context: *mut Context,
    pending: OperationHandle,
    output: *mut abi::Page,
) -> i64 {
    // SAFETY: admit arrays before inspecting a retained completion.
    unsafe {
        with_context(context, |state| {
            pending_data::admit_take(state, operation(pending), rt::Opcode::ObjectQueryTake)?;
            let descriptor = super::record::region(output, true)?;
            let mut result = super::record::read(output, false)?;
            super::record::outputs(&[
                descriptor,
                output_span(&result.objects(), contract::MAX_PAGE_OBJECTS as usize)?,
                output_span(
                    &result.fields(),
                    (contract::MAX_PAGE_OBJECTS * contract::MAX_FIELDS) as usize,
                )?,
                output_span(&result.values(), contract::MAX_PAGE_VALUES as usize)?,
                output_span(&result.bytes(), contract::MAX_DATA_BYTES)?,
            ])?;
            playbit_api::abi::record::clear_native(output)?;
            super::record::clear_array(&result.objects());
            super::record::clear_array(&result.fields());
            super::record::clear_array(&result.values());
            pending_data::take(
                state,
                operation(pending),
                rt::Opcode::ObjectQueryTake,
                |call, data| {
                    let (DataOperation::Query { type_id, projection, .. }, DataResult::Page(page)) =
                        (&call.operation, data)
                    else {
                        return Err(rt::HostError::Internal);
                    };
                    super::objects::counts(page, &mut result);
                    if result.objects_count > result.objects_capacity
                        || result.fields_count > result.fields_capacity
                        || result.values_count > result.values_capacity
                        || result.bytes_count > result.bytes_capacity
                    {
                        super::record::counts(output, result)?;
                        return Err(rt::HostError::BufferTooSmall);
                    }
                    super::objects::write_page(page, *type_id, &result)?;
                    result.next = page.next.unwrap_or([0; 16]);
                    result.has_next = u32::from(page.next.is_some());
                    result.projected = u32::from(projection.is_some());
                    super::record::write(output, result)?;
                    Ok(0)
                },
            )
        })
    }
}
