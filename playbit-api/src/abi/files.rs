//! Owned File/FileList capabilities, scoped stored references, and completion records.
use super::operations::Context;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(C)]
pub struct FileReference {
    pub size: u32,
    pub written: u32,
    pub volume: u64,
    pub entry_id: [u8; 15],
    pub reserved: u8,
}
/// Instance-generation scoped File and FileList capabilities. Zero is invalid.
/// These are distinct from persistent entry references used in stored data/UI.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[repr(transparent)]
pub struct FileHandle(pub u64);
impl FileHandle {
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
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[repr(transparent)]
pub struct FileListHandle(pub u64);
impl FileListHandle {
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
pub const FILE_READ: u32 = 1;
pub const FILE_DIRECTORY: u32 = 2;
pub const FILE_WRITE: u32 = 4;
pub const FILE_EXCLUSIVE: u32 = 8;
pub const FILE_CLOSE_SYNC: u32 = 1;
pub const FILE_CLOSE_DISCARD: u32 = 2;
pub const FILE_KIND_FILE: u32 = 1;
pub const FILE_KIND_DIRECTORY: u32 = 2;
pub const FILE_KIND_SYMLINK: u32 = 3;
pub const FILE_KIND_OTHER: u32 = 4;
pub const FILE_SIZE_VALID: u32 = 1;
pub const FILE_MODIFICATION_TIME_VALID: u32 = 2;
pub const FILE_CONTENTS_IMMUTABLE: u32 = 4;
pub const MAX_FILE_PATH_BYTES: usize = 4096;
pub const MAX_FILE_EXTENT: u64 = 64 * 1024 * 1024;
/// Reads can address larger immutable bundle files without enlarging writes.
pub const MAX_FILE_READ_EXTENT: u64 = 256 * 1024 * 1024;
pub const MAX_FILE_READ: u32 = 64 * 1024;
pub const MAX_FILE_WRITE: u32 = 1024 * 1024;

/// Fixed layout on wasm32 and native targets. Unavailable fields are zero.
/// Timestamps are signed Unix microseconds, rounded down from finer precision.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(C)]
pub struct FileStatus {
    pub size: u32,
    pub written: u32,
    pub kind: u32,
    pub flags: u32,
    pub file_size: u64,
    pub modification_time: i64,
}
impl FileStatus {
    pub const fn private(size: u64, immutable: bool) -> Self {
        Self {
            size: core::mem::size_of::<Self>() as u32,
            written: 0,
            kind: FILE_KIND_FILE,
            flags: FILE_SIZE_VALID | if immutable { FILE_CONTENTS_IMMUTABLE } else { 0 },
            file_size: size,
            modification_time: 0,
        }
    }
}
pub const fn valid_open_mode(mode: u32) -> bool {
    mode == FILE_READ
        || mode == FILE_DIRECTORY
        || mode == FILE_WRITE
        || mode == FILE_WRITE | FILE_EXCLUSIVE
}
pub const fn valid_close_flags(flags: u32, writer: bool) -> bool {
    flags == 0 || flags == FILE_CLOSE_DISCARD || flags == FILE_CLOSE_SYNC && writer
}
const _: () = assert!(core::mem::size_of::<FileHandle>() == 8);
const _: () = assert!(core::mem::size_of::<FileListHandle>() == 8);

/// Terminal close outcome. A nonzero error can carry one owned recovery File.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(C)]
pub struct FileCloseResult {
    pub size: u32,
    pub written: u32,
    pub error: i32,
    pub publication: u32,
    pub recovery: u64,
}
pub const FILE_NOT_PUBLISHED: u32 = 0;
pub const FILE_PUBLISHED: u32 = 1;
pub const FILE_PUBLICATION_UNKNOWN: u32 = 2;
pub const FILE_PUBLISHED_DURABILITY_UNKNOWN: u32 = 3;
pub const FILE_LIST_END: u32 = 0;
pub const FILE_LIST_ENTRY: u32 = 1;
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct FileListEntry {
    pub size: u32,
    pub written: u32,
    pub outcome: u32,
    pub kind: u32,
    pub name_data: *mut u8,
    pub name_capacity: u32,
    pub name_count: u32,
}

/// # Safety
/// Context and stable borrowed inputs obey playbit/sys/abi.h.
#[inline]
#[allow(unsafe_code, reason = "Typed lowering to the scalar syscall boundary.")]
pub unsafe fn pb_file_open_begin(
    context: *mut Context,
    file: FileHandle,
    path: *const u8,
    length: u32,
    mode: u32,
) -> i64 {
    // SAFETY: typed storage remains live and disjoint for this bounded call.
    unsafe {
        super::invocation::invoke(
            context,
            super::syscall::Opcode::FileOpenBegin as u32,
            [file.0, path as usize as u64, u64::from(length), u64::from(mode), 0, 0],
        )
    }
}

/// # Safety
/// Context and stable borrowed inputs obey playbit/sys/abi.h.
#[inline]
#[allow(unsafe_code, reason = "Typed lowering to the scalar syscall boundary.")]
pub unsafe fn pb_file_open_take(
    context: *mut Context,
    operation: super::pending::OperationHandle,
) -> i64 {
    // SAFETY: typed storage remains live and disjoint for this bounded call.
    unsafe {
        super::invocation::invoke(
            context,
            super::syscall::Opcode::FileOpenTake as u32,
            [operation.0, 0, 0, 0, 0, 0],
        )
    }
}

/// # Safety
/// Context and stable borrowed inputs obey playbit/sys/abi.h.
#[inline]
#[allow(unsafe_code, reason = "Typed lowering to the scalar syscall boundary.")]
pub unsafe fn pb_file_read_begin(
    context: *mut Context,
    file: FileHandle,
    offset: u64,
    length: u32,
) -> i64 {
    // SAFETY: typed storage remains live and disjoint for this bounded call.
    unsafe {
        super::invocation::invoke(
            context,
            super::syscall::Opcode::FileReadBegin as u32,
            [file.0, offset, u64::from(length), 0, 0, 0],
        )
    }
}

/// # Safety
/// Context and all borrowed input/output spans obey playbit/sys/abi.h.
#[inline]
#[allow(unsafe_code, reason = "Typed lowering to the scalar syscall boundary.")]
pub unsafe fn pb_file_read_take(
    context: *mut Context,
    operation: super::pending::OperationHandle,
    data: *mut u8,
    capacity: u32,
    count: *mut u32,
) -> i64 {
    // SAFETY: typed storage remains live and disjoint for this bounded call.
    unsafe {
        super::invocation::invoke(
            context,
            super::syscall::Opcode::FileReadTake as u32,
            [operation.0, data as usize as u64, capacity as u64, count as usize as u64, 0, 0],
        )
    }
}

/// # Safety
/// Context and stable borrowed inputs obey playbit/sys/abi.h.
#[inline]
#[allow(unsafe_code, reason = "Typed lowering to the scalar syscall boundary.")]
pub unsafe fn pb_file_write_begin(
    context: *mut Context,
    file: FileHandle,
    offset: u64,
    bytes: *const u8,
    length: u32,
) -> i64 {
    // SAFETY: typed storage remains live and disjoint for this bounded call.
    unsafe {
        super::invocation::invoke(
            context,
            super::syscall::Opcode::FileWriteBegin as u32,
            [file.0, offset, bytes as usize as u64, u64::from(length), 0, 0],
        )
    }
}

/// # Safety
/// Context and all borrowed input/output spans obey playbit/sys/abi.h.
#[inline]
#[allow(unsafe_code, reason = "Typed lowering to the scalar syscall boundary.")]
pub unsafe fn pb_file_write_take(
    context: *mut Context,
    operation: super::pending::OperationHandle,
) -> i64 {
    // SAFETY: typed storage remains live and disjoint for this bounded call.
    unsafe {
        super::invocation::invoke(
            context,
            super::syscall::Opcode::FileWriteTake as u32,
            [operation.0, 0, 0, 0, 0, 0],
        )
    }
}

/// # Safety
/// Context and stable borrowed inputs obey playbit/sys/abi.h.
#[inline]
#[allow(unsafe_code, reason = "Typed lowering to the scalar syscall boundary.")]
pub unsafe fn pb_file_status_begin(context: *mut Context, file: FileHandle) -> i64 {
    // SAFETY: typed storage remains live and disjoint for this bounded call.
    unsafe {
        super::invocation::invoke(
            context,
            super::syscall::Opcode::FileStatusBegin as u32,
            [file.0, 0, 0, 0, 0, 0],
        )
    }
}

/// # Safety
/// Context and all borrowed input/output spans obey playbit/sys/abi.h.
#[inline]
#[allow(unsafe_code, reason = "Typed lowering to the scalar syscall boundary.")]
pub unsafe fn pb_file_status_take(
    context: *mut Context,
    operation: super::pending::OperationHandle,
    output: *mut FileStatus,
) -> i64 {
    // SAFETY: typed storage remains live and disjoint for this bounded call.
    unsafe {
        super::invocation::invoke(
            context,
            super::syscall::Opcode::FileStatusTake as u32,
            [operation.0, output as usize as u64, 0, 0, 0, 0],
        )
    }
}

/// # Safety
/// Context and stable borrowed inputs obey playbit/sys/abi.h.
#[inline]
#[allow(unsafe_code, reason = "Typed lowering to the scalar syscall boundary.")]
pub unsafe fn pb_file_close_begin(context: *mut Context, file: FileHandle, flags: u32) -> i64 {
    // SAFETY: typed storage remains live and disjoint for this bounded call.
    unsafe {
        super::invocation::invoke(
            context,
            super::syscall::Opcode::FileCloseBegin as u32,
            [file.0, u64::from(flags), 0, 0, 0, 0],
        )
    }
}

/// # Safety
/// Context and all borrowed input/output spans obey playbit/sys/abi.h.
#[inline]
#[allow(unsafe_code, reason = "Typed lowering to the scalar syscall boundary.")]
pub unsafe fn pb_file_close_take(
    context: *mut Context,
    operation: super::pending::OperationHandle,
    output: *mut FileCloseResult,
) -> i64 {
    // SAFETY: typed storage remains live and disjoint for this bounded call.
    unsafe {
        super::invocation::invoke(
            context,
            super::syscall::Opcode::FileCloseTake as u32,
            [operation.0, output as usize as u64, 0, 0, 0, 0],
        )
    }
}

/// # Safety
/// Context and stable borrowed inputs obey playbit/sys/abi.h.
#[inline]
#[allow(unsafe_code, reason = "Typed lowering to the scalar syscall boundary.")]
pub unsafe fn pb_file_list_open_begin(context: *mut Context, file: FileHandle, flags: u32) -> i64 {
    // SAFETY: typed storage remains live and disjoint for this bounded call.
    unsafe {
        super::invocation::invoke(
            context,
            super::syscall::Opcode::FileListOpenBegin as u32,
            [file.0, u64::from(flags), 0, 0, 0, 0],
        )
    }
}

/// # Safety
/// Context and stable borrowed inputs obey playbit/sys/abi.h.
#[inline]
#[allow(unsafe_code, reason = "Typed lowering to the scalar syscall boundary.")]
pub unsafe fn pb_file_list_open_take(
    context: *mut Context,
    operation: super::pending::OperationHandle,
) -> i64 {
    // SAFETY: typed storage remains live and disjoint for this bounded call.
    unsafe {
        super::invocation::invoke(
            context,
            super::syscall::Opcode::FileListOpenTake as u32,
            [operation.0, 0, 0, 0, 0, 0],
        )
    }
}

/// # Safety
/// Context and stable borrowed inputs obey playbit/sys/abi.h.
#[inline]
#[allow(unsafe_code, reason = "Typed lowering to the scalar syscall boundary.")]
pub unsafe fn pb_file_list_next_begin(context: *mut Context, list: FileListHandle) -> i64 {
    // SAFETY: typed storage remains live and disjoint for this bounded call.
    unsafe {
        super::invocation::invoke(
            context,
            super::syscall::Opcode::FileListNextBegin as u32,
            [list.0, 0, 0, 0, 0, 0],
        )
    }
}

/// # Safety
/// Context and all borrowed input/output spans obey playbit/sys/abi.h.
#[inline]
#[allow(unsafe_code, reason = "Typed lowering to the scalar syscall boundary.")]
pub unsafe fn pb_file_list_next_take(
    context: *mut Context,
    operation: super::pending::OperationHandle,
    output: *mut FileListEntry,
) -> i64 {
    // SAFETY: typed storage remains live and disjoint for this bounded call.
    unsafe {
        super::invocation::invoke(
            context,
            super::syscall::Opcode::FileListNextTake as u32,
            [operation.0, output as usize as u64, 0, 0, 0, 0],
        )
    }
}

/// # Safety
/// Context and all borrowed input/output spans obey playbit/sys/abi.h.
#[inline]
#[allow(unsafe_code, reason = "Typed lowering to the scalar syscall boundary.")]
pub unsafe fn pb_file_list_close(context: *mut Context, list: FileListHandle) -> i64 {
    // SAFETY: typed storage remains live and disjoint for this bounded call.
    unsafe {
        super::invocation::invoke(
            context,
            super::syscall::Opcode::FileListClose as u32,
            [list.0, 0, 0, 0, 0, 0],
        )
    }
}

/// # Safety
/// Context is borrowed from the current serialized guest callback.
#[inline]
#[allow(unsafe_code, reason = "Typed lowering to the scalar syscall boundary.")]
pub unsafe fn pb_operation_dispose(
    context: *mut Context,
    operation: super::pending::OperationHandle,
) -> i64 {
    // SAFETY: only the live callback context is borrowed; no pointer is retained.
    unsafe {
        super::invocation::invoke(
            context,
            super::syscall::Opcode::OperationDispose as u32,
            [operation.0, 0, 0, 0, 0, 0],
        )
    }
}

/// Discard a File without a result. Accepted work remains ordered and retained.
/// # Safety
/// Context is borrowed from the current serialized guest callback.
#[inline]
#[allow(unsafe_code, reason = "Typed lowering to the scalar syscall boundary.")]
pub unsafe fn pb_file_dispose(context: *mut Context, file: FileHandle) -> i64 {
    // SAFETY: only the live callback context is borrowed; no pointer is retained.
    unsafe {
        super::invocation::invoke(
            context,
            super::syscall::Opcode::FileDispose as u32,
            [file.0, 0, 0, 0, 0, 0],
        )
    }
}

impl Default for FileReference {
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

super::record::record!(FileReference, 32, { volume: u64, entry_id: [u8; 15], reserved: u8 });

impl Default for FileStatus {
    fn default() -> Self {
        Self {
            size: core::mem::size_of::<Self>() as u32,
            written: 0,
            kind: 0,
            flags: 0,
            file_size: 0,
            modification_time: 0,
        }
    }
}

super::record::record!(FileStatus, 32, { kind: u32, flags: u32, file_size: u64, modification_time: i64 });

impl Default for FileCloseResult {
    fn default() -> Self {
        Self {
            size: core::mem::size_of::<Self>() as u32,
            written: 0,
            error: 0,
            publication: 0,
            recovery: 0,
        }
    }
}

super::record::record!(FileCloseResult, 24, { error: i32, publication: u32, recovery: u64 });

super::record::record!(FileListEntry, if cfg!(target_pointer_width = "64") { 32 } else { 28 }, { outcome: u32, kind: u32, name_data: *mut u8, name_capacity: u32, name_count: u32 });
