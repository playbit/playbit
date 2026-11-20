//! One atomic authenticated field snapshot. Unused metadata members are zero.
use super::data::Value;
use super::{operations::Context, pending::OperationHandle};
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(C)]
pub struct Metadata {
    pub size: u32,
    pub written: u32,
    pub volume_id: [u8; 16],
    pub object_id: [u8; 16],
    pub type_id: [u8; 16],
    pub field_id: [u8; 16],
    pub revision: [u8; 32],
    pub principal_id: [u8; 16],
    pub device_id: [u8; 16],
    pub state: u32,
    pub provenance: u32,
    pub principal_kind: u32,
    pub reserved: u32,
    pub entry_id: [u8; 15],
    pub entry_reserved: u8,
}
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct Field {
    pub size: u32,
    pub written: u32,
    pub metadata: *mut Metadata,
    pub values_data: *mut Value,
    pub values_capacity: u32,
    pub values_count: u32,
    pub values_stride: u32,
    pub bytes_data: *mut u8,
    pub bytes_capacity: u32,
    pub bytes_count: u32,
}
/// # Safety
/// Context and all pointers obey the borrowed operation contract in playbit/sys/abi.h.
#[inline]
#[allow(unsafe_code, reason = "Typed lowering to the scalar C syscall boundary.")]
pub unsafe fn pb_object_field_read_authenticated_begin(
    context: *mut Context,
    entry: *const super::data::EntryHandle,
    object_id: *const [u8; 16],
    field_id: *const [u8; 16],
) -> i64 {
    // SAFETY: the caller supplies valid native pointers or guest offsets for this call.
    unsafe {
        super::invocation::invoke(
            context,
            super::syscall::Opcode::ObjectFieldReadAuthenticatedBegin as u32,
            [entry as usize as u64, object_id as usize as u64, field_id as usize as u64, 0, 0, 0],
        )
    }
}
/// # Safety
/// The initialized descriptor and disjoint output slices obey playbit/sys/abi.h.
#[inline]
#[allow(unsafe_code, reason = "Typed lowering to the scalar C syscall boundary.")]
pub unsafe fn pb_object_field_read_authenticated_take(
    context: *mut Context,
    handle: OperationHandle,
    output: *mut Field,
) -> i64 {
    // SAFETY: the caller supplies live output storage for this invocation.
    unsafe {
        super::invocation::invoke(
            context,
            super::syscall::Opcode::ObjectFieldReadAuthenticatedTake as u32,
            [handle.0, output as usize as u64, 0, 0, 0, 0],
        )
    }
}

impl Default for Metadata {
    fn default() -> Self {
        Self {
            size: core::mem::size_of::<Self>() as u32,
            written: 0,
            volume_id: [0; 16],
            object_id: [0; 16],
            type_id: [0; 16],
            field_id: [0; 16],
            revision: [0; 32],
            principal_id: [0; 16],
            device_id: [0; 16],
            state: 0,
            provenance: 0,
            principal_kind: 0,
            reserved: 0,
            entry_id: [0; 15],
            entry_reserved: 0,
        }
    }
}

super::record::record!(Metadata, 168, { volume_id: [u8; 16], object_id: [u8; 16], type_id: [u8; 16], field_id: [u8; 16], revision: [u8; 32], principal_id: [u8; 16], device_id: [u8; 16], state: u32, provenance: u32, principal_kind: u32, reserved: u32, entry_id: [u8; 15], entry_reserved: u8 });

super::record::record!(Field, if cfg!(target_pointer_width = "64") { 56 } else { 40 }, { metadata: *mut Metadata, values_data: *mut Value, values_capacity: u32, values_count: u32, values_stride: u32, bytes_data: *mut u8, bytes_capacity: u32, bytes_count: u32 });
