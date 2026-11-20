//! Fixed-layout object and volume records for operation arguments/results.
//! Output ranges index caller-owned arrays, never host memory.
use super::operations::Context;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[repr(transparent)]
pub struct VolumeHandle(pub u64);
impl VolumeHandle {
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

/// A stable entry ID inside an instance-scoped volume capability. The ID names
/// data and never adds authority; hosts recheck entry permissions on each call.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(C)]
pub struct EntryHandle {
    pub size: u32,
    pub written: u32,
    pub volume: u64,
    pub entry_id: [u8; 15],
    pub reserved: u8,
}
impl EntryHandle {
    pub const fn volume_handle(&self) -> VolumeHandle {
        let (generation, slot) = super::dispatch::unpack_handle(self.volume);
        VolumeHandle::new(generation, slot)
    }
    pub fn valid(&self) -> bool {
        self.reserved == 0 && self.entry_id.iter().any(|byte| *byte != 0)
    }
}

/// # Safety
/// Context and all pointers obey the borrowed operation contract in playbit/sys/abi.h.
#[inline]
#[allow(unsafe_code, reason = "Typed lowering to the scalar C syscall boundary.")]
pub unsafe fn pb_entry_resolve_begin(
    context: *mut Context,
    volume: VolumeHandle,
    name: *const u8,
    length: u32,
) -> i64 {
    // SAFETY: the caller supplies valid native pointers or guest offsets for this call.
    unsafe {
        super::invocation::invoke(
            context,
            super::syscall::Opcode::EntryResolveBegin as u32,
            [volume.0, name as usize as u64, length as u64, 0, 0, 0],
        )
    }
}

/// # Safety
/// Context and the disjoint writable entry obey the borrowed C contract.
#[inline]
#[allow(unsafe_code, reason = "Typed lowering to the scalar C syscall boundary.")]
pub unsafe fn pb_entry_resolve_take(
    context: *mut Context,
    handle: super::pending::OperationHandle,
    output: *mut EntryHandle,
) -> i64 {
    // SAFETY: the caller owns the live output and context for this call.
    unsafe {
        super::invocation::invoke(
            context,
            super::syscall::Opcode::EntryResolveTake as u32,
            [handle.0, output as usize as u64, 0, 0, 0, 0],
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(C)]
pub struct Range {
    pub size: u32,
    pub written: u32,
    pub offset: u32,
    pub count: u32,
}
pub const VOLUME_ROOT: u32 = 1;
pub const VOLUME_BACKING_MASK: u32 = 0xff00;
pub const VOLUME_PLAYBIT: u32 = 0x100;
pub const VOLUME_HOST_FS: u32 = 0x200;
pub const VOLUME_BUNDLE: u32 = 0x300;
pub const SESSION_VOLUME_FEATURE: &str = "session-volume.v1";
pub const AUTOMATIC_VOLUMES_FEATURE: &str = "automatic-volumes.v1";

/// Host-assigned automatic scope. Zero in the ABI means an explicit mount.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum VolumeRole {
    Root = 1,
    Session = 2,
    User = 3,
    UserSession = 4,
    Local = 5,
    LocalSession = 6,
}
impl VolumeRole {
    pub const fn from_raw(value: u32) -> Option<Self> {
        match value {
            1 => Some(Self::Root),
            2 => Some(Self::Session),
            3 => Some(Self::User),
            4 => Some(Self::UserSession),
            5 => Some(Self::Local),
            6 => Some(Self::LocalSession),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(C)]
pub struct Volume {
    pub size: u32,
    pub written: u32,
    pub handle: u64,
    pub id: [u8; 16],
    pub name_offset: u32,
    pub name_count: u32,
    pub tags_offset: u32,
    pub tags_count: u32,
    pub rights: u32,
    pub flags: u32,
    pub file: u64,
    pub grant_id: [u8; 16],
    pub role: u32,
}
#[derive(Clone, Copy, Debug)]
pub struct Buffer<T> {
    pub data: *mut T,
    pub capacity: u32,
    pub count: u32,
    pub stride: u32,
}

impl<T> Buffer<T> {
    pub const fn new(data: *mut T, capacity: u32) -> Self {
        Self { data, capacity, count: 0, stride: core::mem::size_of::<T>() as u32 }
    }
}
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct Volumes {
    pub size: u32,
    pub written: u32,
    pub revision: u64,
    pub volumes_data: *mut Volume,
    pub volumes_capacity: u32,
    pub volumes_count: u32,
    pub volumes_stride: u32,
    pub tags_data: *mut Range,
    pub tags_capacity: u32,
    pub tags_count: u32,
    pub tags_stride: u32,
    pub bytes_data: *mut u8,
    pub bytes_capacity: u32,
    pub bytes_count: u32,
}

pub const TEXT: u32 = 1;
pub const BOOLEAN: u32 = 2;
pub const INTEGER: u32 = 3;
pub const FLOAT: u32 = 4;
pub const BYTES: u32 = 5;
pub const REFERENCE: u32 = 6;
pub const FILE: u32 = 7;

#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct ValueInput {
    pub size: u32,
    pub written: u32,
    pub kind: u32,
    pub length: u32,
    pub scalar: u64,
    pub id1: [u8; 16],
    pub id2: [u8; 16],
    pub entry_id: [u8; 15],
    pub reserved: u8,
    pub data: *const u8,
}
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct FieldInput {
    pub size: u32,
    pub written: u32,
    pub id: [u8; 16],
    pub values: *const ValueInput,
    pub count: u32,
    pub stride: u32,
    pub present: u32,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(C)]
pub struct Value {
    pub size: u32,
    pub written: u32,
    pub kind: u32,
    pub reserved: u32,
    pub scalar: u64,
    pub id1: [u8; 16],
    pub id2: [u8; 16],
    pub entry_id: [u8; 15],
    pub entry_reserved: u8,
    pub bytes_offset: u32,
    pub bytes_count: u32,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(C)]
pub struct Field {
    pub size: u32,
    pub written: u32,
    pub id: [u8; 16],
    pub values_offset: u32,
    pub values_count: u32,
    pub present: u32,
    pub reserved: u32,
}
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct Object {
    pub size: u32,
    pub written: u32,
    pub type_id: [u8; 16],
    pub fields_data: *mut Field,
    pub fields_capacity: u32,
    pub fields_count: u32,
    pub fields_stride: u32,
    pub values_data: *mut Value,
    pub values_capacity: u32,
    pub values_count: u32,
    pub values_stride: u32,
    pub bytes_data: *mut u8,
    pub bytes_capacity: u32,
    pub bytes_count: u32,
}

#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct Query {
    pub size: u32,
    pub written: u32,
    pub type_id: [u8; 16],
    pub after: [u8; 16],
    pub flags: u32,
    pub limit: u32,
    pub fields: *const [u8; 16],
    pub count: u32,
    pub reserved: u32,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(C)]
pub struct ObjectInfo {
    pub size: u32,
    pub written: u32,
    pub id: [u8; 16],
    pub type_id: [u8; 16],
    pub fields_offset: u32,
    pub fields_count: u32,
}
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct Page {
    pub size: u32,
    pub written: u32,
    pub next: [u8; 16],
    pub has_next: u32,
    pub projected: u32,
    pub objects_data: *mut ObjectInfo,
    pub objects_capacity: u32,
    pub objects_count: u32,
    pub objects_stride: u32,
    pub fields_data: *mut Field,
    pub fields_capacity: u32,
    pub fields_count: u32,
    pub fields_stride: u32,
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
pub unsafe fn pb_volumes(context: *mut Context, output: *mut Volumes) -> i64 {
    // SAFETY: the caller supplies valid native pointers or guest offsets for this call.
    unsafe {
        super::invocation::invoke(
            context,
            super::syscall::Opcode::AvailableVolumes as u32,
            [output as usize as u64, 0, 0, 0, 0, 0],
        )
    }
}

/// Register one root-set notification after `revision`. Re-register after enumeration.
/// # Safety
/// Context obeys the borrowed C operation contract.
#[inline]
#[allow(unsafe_code, reason = "Typed lowering to the scalar C syscall boundary.")]
pub unsafe fn pb_volumes_observe(context: *mut Context, revision: u64) -> i64 {
    // SAFETY: the caller supplies a live context; there are no borrowed buffers.
    unsafe {
        super::invocation::invoke(
            context,
            super::syscall::Opcode::VolumesObserve as u32,
            [revision, 0, 0, 0, 0, 0],
        )
    }
}

impl Default for EntryHandle {
    fn default() -> Self {
        Self {
            size: core::mem::size_of::<Self>() as u32,
            written: 0,
            volume: 0,
            entry_id: [0; 15],
            reserved: 0,
        }
    }
}

super::record::record!(EntryHandle, 32, { volume: u64, entry_id: [u8; 15], reserved: u8 });

impl Default for Range {
    fn default() -> Self {
        Self { size: core::mem::size_of::<Self>() as u32, written: 0, offset: 0, count: 0 }
    }
}

super::record::record!(Range, 16, { offset: u32, count: u32 });

impl Default for Volume {
    fn default() -> Self {
        Self {
            size: core::mem::size_of::<Self>() as u32,
            written: 0,
            handle: 0,
            id: [0; 16],
            name_offset: 0,
            name_count: 0,
            tags_offset: 0,
            tags_count: 0,
            rights: 0,
            flags: 0,
            file: 0,
            grant_id: [0; 16],
            role: 0,
        }
    }
}

super::record::record!(Volume, 80, { handle: u64, id: [u8; 16], name_offset: u32, name_count: u32, tags_offset: u32, tags_count: u32, rights: u32, flags: u32, file: u64, grant_id: [u8; 16], role: u32 });

super::record::record!(Volumes, if cfg!(target_pointer_width = "64") { 80 } else { 64 }, { revision: u64, volumes_data: *mut Volume, volumes_capacity: u32, volumes_count: u32, volumes_stride: u32, tags_data: *mut Range, tags_capacity: u32, tags_count: u32, tags_stride: u32, bytes_data: *mut u8, bytes_capacity: u32, bytes_count: u32 });

super::record::record!(ValueInput, 80, { kind: u32, length: u32, scalar: u64, id1: [u8; 16], id2: [u8; 16], entry_id: [u8; 15], reserved: u8, data: *const u8 });

super::record::record!(FieldInput, if cfg!(target_pointer_width = "64") { 48 } else { 40 }, { id: [u8; 16], values: *const ValueInput, count: u32, stride: u32, present: u32 });

impl Default for Value {
    fn default() -> Self {
        Self {
            size: core::mem::size_of::<Self>() as u32,
            written: 0,
            kind: 0,
            reserved: 0,
            scalar: 0,
            id1: [0; 16],
            id2: [0; 16],
            entry_id: [0; 15],
            entry_reserved: 0,
            bytes_offset: 0,
            bytes_count: 0,
        }
    }
}

super::record::record!(Value, 80, { kind: u32, reserved: u32, scalar: u64, id1: [u8; 16], id2: [u8; 16], entry_id: [u8; 15], entry_reserved: u8, bytes_offset: u32, bytes_count: u32 });

impl Default for Field {
    fn default() -> Self {
        Self {
            size: core::mem::size_of::<Self>() as u32,
            written: 0,
            id: [0; 16],
            values_offset: 0,
            values_count: 0,
            present: 0,
            reserved: 0,
        }
    }
}

super::record::record!(Field, 40, { id: [u8; 16], values_offset: u32, values_count: u32, present: u32, reserved: u32 });

super::record::record!(Object, if cfg!(target_pointer_width = "64") { 88 } else { 68 }, { type_id: [u8; 16], fields_data: *mut Field, fields_capacity: u32, fields_count: u32, fields_stride: u32, values_data: *mut Value, values_capacity: u32, values_count: u32, values_stride: u32, bytes_data: *mut u8, bytes_capacity: u32, bytes_count: u32 });

super::record::record!(Query, if cfg!(target_pointer_width = "64") { 64 } else { 60 }, { type_id: [u8; 16], after: [u8; 16], flags: u32, limit: u32, fields: *const [u8; 16], count: u32, reserved: u32 });

impl Default for ObjectInfo {
    fn default() -> Self {
        Self {
            size: core::mem::size_of::<Self>() as u32,
            written: 0,
            id: [0; 16],
            type_id: [0; 16],
            fields_offset: 0,
            fields_count: 0,
        }
    }
}

super::record::record!(ObjectInfo, 48, { id: [u8; 16], type_id: [u8; 16], fields_offset: u32, fields_count: u32 });

super::record::record!(Page, if cfg!(target_pointer_width = "64") { 120 } else { 92 }, { next: [u8; 16], has_next: u32, projected: u32, objects_data: *mut ObjectInfo, objects_capacity: u32, objects_count: u32, objects_stride: u32, fields_data: *mut Field, fields_capacity: u32, fields_count: u32, fields_stride: u32, values_data: *mut Value, values_capacity: u32, values_count: u32, values_stride: u32, bytes_data: *mut u8, bytes_capacity: u32, bytes_count: u32 });
