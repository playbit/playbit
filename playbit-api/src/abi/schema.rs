//! Schema input pointers and caller-owned result arrays for named operations.
use super::operations::Context;

#[derive(Clone, Copy, Debug)]
pub struct Text {
    pub data: *const u8,
    pub length: u32,
    pub reserved: u32,
}
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct FieldInput {
    pub size: u32,
    pub written: u32,
    pub id: [u8; 16],
    pub name_data: *const u8,
    pub name_length: u32,
    pub kind: u32,
    pub flags: u32,
    pub reference_type: [u8; 16],
}
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct TypeInput {
    pub size: u32,
    pub written: u32,
    pub id: [u8; 16],
    pub name_data: *const u8,
    pub name_length: u32,
    pub fields: *const FieldInput,
    pub count: u32,
    pub stride: u32,
    pub flags: u32,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(C)]
pub struct Field {
    pub size: u32,
    pub written: u32,
    pub id: [u8; 16],
    pub name_offset: u32,
    pub name_count: u32,
    pub kind: u32,
    pub flags: u32,
    pub reference_type: [u8; 16],
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(C)]
pub struct Type {
    pub size: u32,
    pub written: u32,
    pub id: [u8; 16],
    pub name_offset: u32,
    pub name_count: u32,
    pub fields_offset: u32,
    pub fields_count: u32,
    pub flags: u32,
    pub reserved: u32,
}
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct Schema {
    pub size: u32,
    pub written: u32,
    pub types_data: *mut Type,
    pub types_capacity: u32,
    pub types_count: u32,
    pub types_stride: u32,
    pub fields_data: *mut Field,
    pub fields_capacity: u32,
    pub fields_count: u32,
    pub fields_stride: u32,
    pub bytes_data: *mut u8,
    pub bytes_capacity: u32,
    pub bytes_count: u32,
}

/// # Safety
/// Context and stable borrowed inputs obey playbit/sys/abi.h.
#[inline]
#[allow(unsafe_code, reason = "Typed lowering to the scalar C syscall boundary.")]
pub unsafe fn pb_schema_read_begin(
    context: *mut Context,
    entry: *const super::data::EntryHandle,
) -> i64 {
    // SAFETY: borrowed typed arguments remain valid for the call.
    unsafe {
        super::invocation::invoke(
            context,
            super::syscall::Opcode::SchemaReadBegin as u32,
            [entry as usize as u64, 0, 0, 0, 0, 0],
        )
    }
}
/// # Safety
/// Context and disjoint result arrays obey the borrowed C contract.
#[inline]
#[allow(unsafe_code, reason = "Typed lowering to the scalar C syscall boundary.")]
pub unsafe fn pb_schema_read_take(
    context: *mut Context,
    pending: super::pending::OperationHandle,
    output: *mut Schema,
) -> i64 {
    // SAFETY: borrowed typed arguments remain valid for the call.
    unsafe {
        super::invocation::invoke(
            context,
            super::syscall::Opcode::SchemaReadTake as u32,
            [pending.0, output as usize as u64, 0, 0, 0, 0],
        )
    }
}
/// # Safety
/// Context and stable borrowed inputs obey playbit/sys/abi.h.
#[inline]
#[allow(unsafe_code, reason = "Typed lowering to the scalar C syscall boundary.")]
pub unsafe fn pb_schema_establish_begin(
    context: *mut Context,
    entry: *const super::data::EntryHandle,
    types: *const TypeInput,
    count: u32,
    stride: u32,
) -> i64 {
    // SAFETY: borrowed typed arguments remain valid for the call.
    unsafe {
        super::invocation::invoke(
            context,
            super::syscall::Opcode::SchemaEstablishBegin as u32,
            [entry as usize as u64, types as usize as u64, u64::from(count), stride as u64, 0, 0],
        )
    }
}
/// # Safety
/// Context obeys the borrowed C contract; there is no pointer output.
#[inline]
#[allow(unsafe_code, reason = "Typed lowering to the scalar C syscall boundary.")]
pub unsafe fn pb_schema_establish_take(
    context: *mut Context,
    pending: super::pending::OperationHandle,
) -> i64 {
    // SAFETY: borrowed context remains valid for the call.
    unsafe {
        super::invocation::invoke(
            context,
            super::syscall::Opcode::SchemaEstablishTake as u32,
            [pending.0, 0, 0, 0, 0, 0],
        )
    }
}

super::record::record!(FieldInput, if cfg!(target_pointer_width = "64") { 64 } else { 56 }, { id: [u8; 16], name_data: *const u8, name_length: u32, kind: u32, flags: u32, reference_type: [u8; 16] });

super::record::record!(TypeInput, if cfg!(target_pointer_width = "64") { 64 } else { 48 }, { id: [u8; 16], name_data: *const u8, name_length: u32, fields: *const FieldInput, count: u32, stride: u32, flags: u32 });

impl Default for Field {
    fn default() -> Self {
        Self {
            size: core::mem::size_of::<Self>() as u32,
            written: 0,
            id: [0; 16],
            name_offset: 0,
            name_count: 0,
            kind: 0,
            flags: 0,
            reference_type: [0; 16],
        }
    }
}

super::record::record!(Field, 56, { id: [u8; 16], name_offset: u32, name_count: u32, kind: u32, flags: u32, reference_type: [u8; 16] });

impl Default for Type {
    fn default() -> Self {
        Self {
            size: core::mem::size_of::<Self>() as u32,
            written: 0,
            id: [0; 16],
            name_offset: 0,
            name_count: 0,
            fields_offset: 0,
            fields_count: 0,
            flags: 0,
            reserved: 0,
        }
    }
}

super::record::record!(Type, 48, { id: [u8; 16], name_offset: u32, name_count: u32, fields_offset: u32, fields_count: u32, flags: u32, reserved: u32 });

super::record::record!(Schema, if cfg!(target_pointer_width = "64") { 72 } else { 52 }, { types_data: *mut Type, types_capacity: u32, types_count: u32, types_stride: u32, fields_data: *mut Field, fields_capacity: u32, fields_count: u32, fields_stride: u32, bytes_data: *mut u8, bytes_capacity: u32, bytes_count: u32 });
