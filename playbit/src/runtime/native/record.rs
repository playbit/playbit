//! Native record admission copies complete fields without borrowing a newest
//! layout from caller storage. Actual allocation validity remains the C contract.
use super::memory::span_at;
use crate::runtime::syscall::{HostError, HostResult};
use playbit_api::abi::{
    record::{self, Element, Record},
    syscall::MAX_QUERY_SCRATCH_BYTES,
};
use std::{mem::align_of, ops::Range};

pub(super) unsafe fn region<T: Record>(
    pointer: *const T,
    output: bool,
) -> HostResult<Range<usize>> {
    // SAFETY: the native caller lends a stable allocation for its declared size.
    unsafe { record::native_region(pointer, output) }
}

pub(super) unsafe fn read<T: Record>(pointer: *const T, input: bool) -> HostResult<T> {
    // SAFETY: all runtime Record declarations contain only zero-valid C fields.
    unsafe { record::read_native(pointer, input) }
}

pub(super) unsafe fn write<T: Record>(pointer: *mut T, value: T) -> HostResult<()> {
    // SAFETY: callers admit every disjoint output before effects or consumption.
    unsafe { record::write_native(pointer, value) }
}

pub(super) unsafe fn clear_array<T: Element>(buffer: &playbit_api::abi::data::Buffer<T>) {
    if !T::RECORD {
        return;
    }
    for index in 0..buffer.capacity as usize {
        // SAFETY: callers admit every array slot and disjoint output before clearing.
        unsafe {
            buffer
                .data
                .cast::<u8>()
                .add(index * buffer.stride as usize)
                .cast::<u32>()
                .add(1)
                .write(0);
        }
    }
}

pub(super) fn outputs(regions: &[Range<usize>]) -> HostResult<()> {
    let bytes = regions.iter().skip(1).map(Range::len).sum::<usize>();
    if bytes > MAX_QUERY_SCRATCH_BYTES {
        return Err(HostError::LimitExceeded);
    }
    super::data::disjoint(regions)
}

pub(super) unsafe fn array<T: Element>(
    pointer: *mut T,
    capacity: u32,
    stride: u32,
    limit: usize,
) -> HostResult<Range<usize>> {
    if capacity as usize > limit || stride > record::MAX_RECORD_BYTES {
        return Err(HostError::LimitExceeded);
    }
    if stride < T::MIN_SIZE || !stride.is_multiple_of(align_of::<T>() as u32) {
        return Err(HostError::InvalidArgument);
    }
    let length = (capacity as usize)
        .checked_mul(stride as usize)
        .filter(|n| *n <= MAX_QUERY_SCRATCH_BYTES)
        .ok_or(HostError::LimitExceeded)?;
    let region = span_at::<u8>(pointer as usize, length)?;
    if capacity != 0 && !(pointer as usize).is_multiple_of(align_of::<T>()) {
        return Err(HostError::InvalidArgument);
    }
    if T::RECORD {
        for index in 0..capacity as usize {
            // SAFETY: checked stride and aggregate span contain every size header.
            let size =
                unsafe { pointer.cast::<u8>().add(index * stride as usize).cast::<u32>().read() };
            if size < T::MIN_SIZE {
                return Err(HostError::BufferTooSmall);
            }
            if size > stride {
                return Err(HostError::InvalidArgument);
            }
            for field in T::FIELDS {
                if field.offset < size
                    && size < field.offset + field.width
                    && Some(field.offset) != T::OPTIONAL_OUTPUT_FIELD
                {
                    return Err(HostError::InvalidArgument);
                }
            }
        }
    }
    Ok(region)
}

pub(super) unsafe fn element<T: Record>(
    pointer: *mut T,
    stride: u32,
    index: usize,
    value: T,
) -> HostResult<()> {
    // SAFETY: array admission established every slot and initialized header before effects.
    unsafe { write(pointer.cast::<u8>().add(index * stride as usize).cast::<T>(), value) }
}

pub(super) unsafe fn counts<T: Record>(pointer: *mut T, value: T) -> HostResult<()> {
    // SAFETY: the descriptor was admitted before retained-result inspection.
    let region = unsafe { region(pointer, true)? };
    for field in T::FIELDS.iter().filter(|field| field.name.ends_with("_count")) {
        assert_eq!(field.width, 4, "descriptor counts are u32");
        if (field.offset + field.width) as usize <= region.len() {
            // SAFETY: complete scalar field inside the stable caller descriptor.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    (&value as *const T).cast::<u8>().add(field.offset as usize),
                    pointer.cast::<u8>().add(field.offset as usize),
                    4,
                );
            }
        }
    }
    // SAFETY: failure never publishes stale fields as initialized.
    unsafe {
        pointer.cast::<u32>().add(1).write(0);
    }
    Ok(())
}

pub(super) fn input_span<T: Record>(
    pointer: *const T,
    count: u32,
    stride: u32,
    limit: usize,
) -> HostResult<Range<usize>> {
    if count as usize > limit || stride > record::MAX_RECORD_BYTES {
        return Err(HostError::LimitExceeded);
    }
    if stride < T::MIN_SIZE || !stride.is_multiple_of(align_of::<T>() as u32) {
        return Err(HostError::InvalidArgument);
    }
    let length = (count as usize)
        .checked_mul(stride as usize)
        .filter(|n| *n <= MAX_QUERY_SCRATCH_BYTES)
        .ok_or(HostError::LimitExceeded)?;
    if count != 0 && !(pointer as usize).is_multiple_of(align_of::<T>()) {
        return Err(HostError::InvalidArgument);
    }
    span_at::<u8>(pointer as usize, length)
}

pub(super) unsafe fn inputs<T: Record>(
    pointer: *const T,
    count: u32,
    stride: u32,
    limit: usize,
) -> HostResult<Vec<T>> {
    input_span(pointer, count, stride, limit)?;
    let mut values = Vec::with_capacity(count as usize);
    for index in 0..count as usize {
        // SAFETY: the checked aggregate span contains every caller-sized slot.
        let item = unsafe { pointer.cast::<u8>().add(index * stride as usize).cast::<T>() };
        // SAFETY: each bounded declared record remains inside its stride slot.
        let region = unsafe { region(item, false)? };
        if region.len() > stride as usize {
            return Err(HostError::InvalidArgument);
        }
        // SAFETY: the slot is admitted before field copying and extension checks.
        values.push(unsafe { read(item, true)? });
    }
    Ok(values)
}
