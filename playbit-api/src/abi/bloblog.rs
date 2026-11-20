//! Bounded append inputs and borrowed log pages. C records borrow caller memory;
//! page frames contain no pointers and use little-endian integers.
use super::syscall::{Error, Reader, Writer};

pub const MAX_BYTES: usize = 64 * 1024;
pub const MAX_BLOBS: usize = 128;
pub const MAX_BLOB_BYTES: usize = 32 * 1024;
pub const MAX_CURSOR_BYTES: usize = 104;
pub const MAX_PAGE_BYTES: usize = 4 + MAX_CURSOR_BYTES + 1 + MAX_BYTES;

const _: () = assert!(MAX_PAGE_BYTES <= super::syscall::MAX_CALL_BYTES);

#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct Blob {
    pub size: u32,
    pub written: u32,
    pub data: *const u8,
    pub length: u32,
}

#[cfg(target_pointer_width = "64")]
super::record::record!(Blob, 24, { data: *const u8, length: u32 });
#[cfg(target_pointer_width = "32")]
super::record::record!(Blob, 16, { data: *const u8, length: u32 });

impl Default for Blob {
    fn default() -> Self {
        Self {
            size: core::mem::size_of::<Self>() as u32,
            written: 0,
            data: core::ptr::null(),
            length: 0,
        }
    }
}

#[cfg(test)]
mod tests;

/// Includes the stored entry headers, so admission matches storage's batch limit.
pub fn validate_lengths(lengths: impl IntoIterator<Item = usize>) -> Result<(), Error> {
    let mut count = 0;
    let mut bytes = 4;
    for length in lengths {
        if count == MAX_BLOBS || length > MAX_BLOB_BYTES {
            return Err(Error::LimitExceeded);
        }
        count += 1;
        bytes += 59 + length;
        if bytes > MAX_BYTES {
            return Err(Error::LimitExceeded);
        }
    }
    if count == 0 {
        return Err(Error::InvalidArgument);
    }
    Ok(())
}

/// Cursors are opaque, replica-local positions. Only the host interprets them.
pub fn cursor(bytes: &[u8]) -> Result<&str, Error> {
    if bytes.is_empty() || bytes.len() > MAX_CURSOR_BYTES || !bytes.is_ascii() {
        return Err(Error::InvalidArgument);
    }
    core::str::from_utf8(bytes).map_err(|_| Error::InvalidArgument)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Entry<'a> {
    pub log: [u8; 15],
    pub id: [u8; 16],
    /// Origin clock, not the local arrival position used by the cursor.
    pub clock: u64,
    pub nonce: [u8; 16],
    pub data: &'a [u8],
}

fn validate_entry(entry: Entry<'_>) -> Result<(), Error> {
    if entry.log == [0; 15]
        || entry.clock == 0
        || entry.clock > u64::MAX - 2
        || entry.data.len() > MAX_BLOB_BYTES
    {
        return Err(Error::InvalidArgument);
    }
    Ok(())
}

fn page_shape<'a>(
    entries: impl Iterator<Item = Entry<'a>>,
    position: &str,
    more: bool,
) -> Result<(usize, usize), Error> {
    cursor(position.as_bytes())?;
    let mut count = 0;
    let mut bytes = 4;
    for entry in entries {
        if count == MAX_BLOBS {
            return Err(Error::LimitExceeded);
        }
        validate_entry(entry)?;
        count += 1;
        bytes += 59 + entry.data.len();
        if bytes > MAX_BYTES {
            return Err(Error::LimitExceeded);
        }
    }
    if more && count == 0 {
        return Err(Error::InvalidArgument);
    }
    Ok((count, 5 + position.len() + bytes))
}

pub fn page_bytes<'a>(
    entries: impl Iterator<Item = Entry<'a>>,
    cursor: &str,
    more: bool,
) -> Result<usize, Error> {
    page_shape(entries, cursor, more).map(|(_, bytes)| bytes)
}

pub fn encode_page<'a>(
    output: &mut [u8],
    entries: impl Iterator<Item = Entry<'a>> + Clone,
    cursor: &str,
    more: bool,
) -> Result<usize, Error> {
    let (count, length) = page_shape(entries.clone(), cursor, more)?;
    if output.len() < length {
        return Err(Error::BufferTooSmall);
    }
    let mut writer = Writer::new(output);
    writer.string(cursor)?;
    writer.bool(more)?;
    writer.u32(count as u32)?;
    for entry in entries {
        writer.put(&entry.log)?;
        writer.id(entry.id)?;
        writer.u64(entry.clock)?;
        writer.id(entry.nonce)?;
        writer.bytes(entry.data)?;
    }
    assert_eq!(writer.position(), length, "page shape matches its encoding");
    Ok(length)
}

fn read_entry<'a>(reader: &mut Reader<'a>) -> Result<Entry<'a>, Error> {
    let entry = Entry {
        log: reader.take(15)?.try_into().map_err(|_| Error::Internal)?,
        id: reader.id()?,
        clock: reader.u64()?,
        nonce: reader.id()?,
        data: reader.bytes()?,
    };
    validate_entry(entry)?;
    Ok(entry)
}

#[derive(Clone, Debug)]
pub struct Entries<'a> {
    bytes: &'a [u8],
    remaining: usize,
}
impl<'a> Iterator for Entries<'a> {
    type Item = Entry<'a>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            return None;
        }
        let mut reader = Reader::new(self.bytes).expect("decoded page fits the frame limit");
        let entry = read_entry(&mut reader).expect("decode_page validated every entry");
        self.bytes = reader.remaining();
        self.remaining -= 1;
        Some(entry)
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.remaining, Some(self.remaining))
    }
}
impl ExactSizeIterator for Entries<'_> {}

#[derive(Clone, Debug)]
pub struct Page<'a> {
    pub entries: Entries<'a>,
    pub cursor: &'a str,
    pub more: bool,
}

// fuzz: abi_reader
pub fn decode_page(bytes: &[u8]) -> Result<Page<'_>, Error> {
    if bytes.len() > MAX_PAGE_BYTES {
        return Err(Error::LimitExceeded);
    }
    let mut reader = Reader::new(bytes)?;
    let cursor = cursor(reader.bytes()?)?;
    let more = reader.bool()?;
    let count = reader.count(MAX_BLOBS as u32)? as usize;
    let entries = reader.remaining();
    if entries.len() + 4 > MAX_BYTES || count > entries.len() / 59 || (more && count == 0) {
        return Err(Error::InvalidArgument);
    }
    for _ in 0..count {
        read_entry(&mut reader)?;
    }
    reader.finish()?;
    Ok(Page { entries: Entries { bytes: entries, remaining: count }, cursor, more })
}

/// # Safety
/// Context and stable borrowed inputs obey playbit/sys/abi.h.
#[inline]
#[allow(unsafe_code, reason = "Typed lowering to the scalar bloblog syscall boundary.")]
pub unsafe fn pb_bloblog_append_begin(
    context: *mut super::operations::Context,
    volume: super::data::VolumeHandle,
    log: *const u8,
    blobs: *const Blob,
    count: u32,
    stride: u32,
    append_id: *const u8,
) -> i64 {
    // SAFETY: all borrowed regions remain live and stable until return.
    unsafe {
        super::invocation::invoke(
            context,
            super::syscall::Opcode::BloblogAppendBegin as u32,
            [
                volume.0,
                log as usize as u64,
                blobs as usize as u64,
                u64::from(count),
                u64::from(stride),
                append_id as usize as u64,
            ],
        )
    }
}

/// # Safety
/// Context and stable borrowed inputs obey playbit/sys/abi.h.
#[inline]
#[allow(unsafe_code, reason = "Typed lowering to the scalar bloblog syscall boundary.")]
pub unsafe fn pb_bloblog_read_begin(
    context: *mut super::operations::Context,
    volume: super::data::VolumeHandle,
    log: *const u8,
    cursor: *const u8,
    length: u32,
) -> i64 {
    // SAFETY: the host copies the bounded borrowed cursor before returning.
    unsafe {
        super::invocation::invoke(
            context,
            super::syscall::Opcode::BloblogReadBegin as u32,
            [volume.0, log as usize as u64, cursor as usize as u64, u64::from(length), 0, 0],
        )
    }
}

/// # Safety
/// Context and disjoint caller-owned count/bytes obey playbit/sys/abi.h.
#[inline]
#[allow(unsafe_code, reason = "Typed lowering to the scalar bloblog syscall boundary.")]
pub unsafe fn pb_bloblog_append_take(
    context: *mut super::operations::Context,
    handle: super::pending::OperationHandle,
    output: *mut u8,
    capacity: u32,
    count: *mut u32,
) -> i64 {
    // SAFETY: the caller owns the live context and writable regions.
    unsafe {
        super::invocation::invoke(
            context,
            super::syscall::Opcode::BloblogAppendTake as u32,
            [handle.0, output as usize as u64, u64::from(capacity), count as usize as u64, 0, 0],
        )
    }
}

/// # Safety
/// Context and disjoint caller-owned count/bytes obey playbit/sys/abi.h.
#[inline]
#[allow(unsafe_code, reason = "Typed lowering to the scalar bloblog syscall boundary.")]
pub unsafe fn pb_bloblog_read_take(
    context: *mut super::operations::Context,
    handle: super::pending::OperationHandle,
    output: *mut u8,
    capacity: u32,
    count: *mut u32,
) -> i64 {
    // SAFETY: the caller owns the live context and writable regions.
    unsafe {
        super::invocation::invoke(
            context,
            super::syscall::Opcode::BloblogReadTake as u32,
            [handle.0, output as usize as u64, u64::from(capacity), count as usize as u64, 0, 0],
        )
    }
}
