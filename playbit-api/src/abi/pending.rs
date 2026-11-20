//! Generation-scoped operation handles and fixed selection results.
use super::files::FileReference;
use super::operations::Context;
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[repr(transparent)]
pub struct OperationHandle(pub u64);
impl OperationHandle {
    pub const fn new(generation: u32, slot: u32) -> Self {
        Self(super::dispatch::pack_handle(generation, slot))
    }
    pub const fn generation(self) -> u32 {
        (self.0 >> 32) as u32
    }
    pub const fn slot(self) -> u32 {
        self.0 as u32
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(C)]
pub struct Selection {
    pub size: u32,
    pub written: u32,
    pub present: u32,
    pub reserved: u32,
    pub volume: u64,
    pub entry_id: [u8; 15],
    pub entry_reserved: u8,
}
/// # Safety
/// Context and all pointers obey the borrowed operation contract in playbit/sys/abi.h.
#[inline]
#[allow(unsafe_code, reason = "Typed lowering to the scalar C syscall boundary.")]
pub unsafe fn pb_object_create_begin(
    context: *mut Context,
    entry: *const super::data::EntryHandle,
    type_id: *const [u8; 16],
    changes: *const super::data::FieldInput,
    count: u32,
    stride: u32,
) -> i64 {
    // SAFETY: the caller supplies valid native pointers or guest offsets for this call.
    unsafe {
        super::invocation::invoke(
            context,
            super::syscall::Opcode::ObjectCreateBegin as u32,
            [
                entry as usize as u64,
                type_id as usize as u64,
                changes as usize as u64,
                u64::from(count),
                stride as u64,
                0,
            ],
        )
    }
}

/// # Safety
/// Context and all pointers obey the borrowed operation contract in playbit/sys/abi.h.
#[inline]
#[allow(unsafe_code, reason = "Typed lowering to the scalar C syscall boundary.")]
pub unsafe fn pb_object_create_poll(
    context: *mut Context,
    handle: OperationHandle,
    output: *mut [u8; 16],
) -> i64 {
    // SAFETY: the caller supplies valid native pointers or guest offsets for this call.
    unsafe {
        super::invocation::invoke(
            context,
            super::syscall::Opcode::ObjectCreatePoll as u32,
            [handle.0, output as usize as u64, 0, 0, 0, 0],
        )
    }
}

/// # Safety
/// Context and all pointers obey the borrowed operation contract in playbit/sys/abi.h.
#[inline]
#[allow(unsafe_code, reason = "Typed lowering to the scalar C syscall boundary.")]
pub unsafe fn pb_file_copy_begin(
    context: *mut Context,
    source: *const FileReference,
    destination: *const FileReference,
) -> i64 {
    // SAFETY: the caller supplies valid native pointers or guest offsets for this call.
    unsafe {
        super::invocation::invoke(
            context,
            super::syscall::Opcode::FileCopyBegin as u32,
            [source as usize as u64, destination as usize as u64, 0, 0, 0, 0],
        )
    }
}

/// # Safety
/// Context and all pointers obey the borrowed operation contract in playbit/sys/abi.h.
#[inline]
#[allow(unsafe_code, reason = "Typed lowering to the scalar C syscall boundary.")]
pub unsafe fn pb_file_copy_poll(
    context: *mut Context,
    handle: OperationHandle,
    output: *mut [u8; 15],
) -> i64 {
    // SAFETY: the caller supplies valid native pointers or guest offsets for this call.
    unsafe {
        super::invocation::invoke(
            context,
            super::syscall::Opcode::FileCopyPoll as u32,
            [handle.0, output as usize as u64, 0, 0, 0, 0],
        )
    }
}

/// # Safety
/// Context and all pointers obey the borrowed operation contract in playbit/sys/abi.h.
#[inline]
#[allow(unsafe_code, reason = "Typed lowering to the scalar C syscall boundary.")]
pub unsafe fn pb_file_select_begin(context: *mut Context) -> i64 {
    // SAFETY: the caller supplies a live context for this invocation.
    unsafe {
        super::invocation::invoke(
            context,
            super::syscall::Opcode::FileSelectBegin as u32,
            [0, 0, 0, 0, 0, 0],
        )
    }
}

/// # Safety
/// Context and all pointers obey the borrowed operation contract in playbit/sys/abi.h.
#[inline]
#[allow(unsafe_code, reason = "Typed lowering to the scalar C syscall boundary.")]
pub unsafe fn pb_file_select_poll(
    context: *mut Context,
    handle: OperationHandle,
    output: *mut Selection,
) -> i64 {
    // SAFETY: the caller supplies valid native pointers or guest offsets for this call.
    unsafe {
        super::invocation::invoke(
            context,
            super::syscall::Opcode::FileSelectPoll as u32,
            [handle.0, output as usize as u64, 0, 0, 0, 0],
        )
    }
}

/// # Safety
/// Context and stable borrowed inputs obey playbit/sys/abi.h.
#[inline]
#[allow(unsafe_code, reason = "Typed lowering to the scalar C syscall boundary.")]
pub unsafe fn pb_object_read_begin(
    context: *mut Context,
    entry: *const super::data::EntryHandle,
    object: *const [u8; 16],
    fields: *const [u8; 16],
    count: u32,
) -> i64 {
    // SAFETY: borrowed typed arguments remain live for this bounded call.
    unsafe {
        super::invocation::invoke(
            context,
            super::syscall::Opcode::ObjectReadBegin as u32,
            [
                entry as usize as u64,
                object as usize as u64,
                fields as usize as u64,
                u64::from(count),
                0,
                0,
            ],
        )
    }
}

/// # Safety
/// Context and stable borrowed inputs obey playbit/sys/abi.h.
#[inline]
#[allow(unsafe_code, reason = "Typed lowering to the scalar C syscall boundary.")]
pub unsafe fn pb_object_edit_begin(
    context: *mut Context,
    entry: *const super::data::EntryHandle,
    object: *const [u8; 16],
    fields: *const super::data::FieldInput,
    count: u32,
    stride: u32,
) -> i64 {
    // SAFETY: borrowed typed arguments remain live for this bounded call.
    unsafe {
        super::invocation::invoke(
            context,
            super::syscall::Opcode::ObjectEditBegin as u32,
            [
                entry as usize as u64,
                object as usize as u64,
                fields as usize as u64,
                u64::from(count),
                stride as u64,
                0,
            ],
        )
    }
}

/// # Safety
/// Context and stable borrowed inputs obey playbit/sys/abi.h.
#[inline]
#[allow(unsafe_code, reason = "Typed lowering to the scalar C syscall boundary.")]
pub unsafe fn pb_object_query_begin(
    context: *mut Context,
    entry: *const super::data::EntryHandle,
    query: *const super::data::Query,
) -> i64 {
    // SAFETY: borrowed typed arguments remain live for this bounded call.
    unsafe {
        super::invocation::invoke(
            context,
            super::syscall::Opcode::ObjectQueryBegin as u32,
            [entry as usize as u64, query as usize as u64, 0, 0, 0, 0],
        )
    }
}

/// # Safety
/// Context, inputs and disjoint caller-owned outputs obey playbit/sys/abi.h.
#[inline]
#[allow(unsafe_code, reason = "Typed lowering to the scalar C syscall boundary.")]
pub unsafe fn pb_object_read_take(
    context: *mut Context,
    handle: OperationHandle,
    output: *mut super::data::Object,
) -> i64 {
    // SAFETY: borrowed typed arguments remain live for this bounded call.
    unsafe {
        super::invocation::invoke(
            context,
            super::syscall::Opcode::ObjectReadTake as u32,
            [handle.0, output as usize as u64, 0, 0, 0, 0],
        )
    }
}

/// # Safety
/// Context, inputs and disjoint caller-owned outputs obey playbit/sys/abi.h.
#[inline]
#[allow(unsafe_code, reason = "Typed lowering to the scalar C syscall boundary.")]
pub unsafe fn pb_object_edit_take(context: *mut Context, handle: OperationHandle) -> i64 {
    // SAFETY: borrowed typed arguments remain live for this bounded call.
    unsafe {
        super::invocation::invoke(
            context,
            super::syscall::Opcode::ObjectEditTake as u32,
            [handle.0, 0, 0, 0, 0, 0],
        )
    }
}

/// # Safety
/// Context, inputs and disjoint caller-owned outputs obey playbit/sys/abi.h.
#[inline]
#[allow(unsafe_code, reason = "Typed lowering to the scalar C syscall boundary.")]
pub unsafe fn pb_object_query_take(
    context: *mut Context,
    handle: OperationHandle,
    output: *mut super::data::Page,
) -> i64 {
    // SAFETY: borrowed typed arguments remain live for this bounded call.
    unsafe {
        super::invocation::invoke(
            context,
            super::syscall::Opcode::ObjectQueryTake as u32,
            [handle.0, output as usize as u64, 0, 0, 0, 0],
        )
    }
}

impl Default for Selection {
    fn default() -> Self {
        Self {
            size: core::mem::size_of::<Self>() as u32,
            written: 0,
            present: 0,
            reserved: 0,
            volume: 0,
            entry_id: [0; 15],
            entry_reserved: 0,
        }
    }
}

super::record::record!(Selection, 40, { present: u32, reserved: u32, volume: u64, entry_id: [u8; 15], entry_reserved: u8 });
