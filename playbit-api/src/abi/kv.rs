//! Ordinary KV calls use bounded binary keys and caller-owned byte frames.
//! All frame integers are little-endian; frames contain no pointers or padding.
use super::syscall::{Error, Reader, Writer};

pub const MAX_KEY_BYTES: usize = 511;
pub const MAX_KEYS: usize = 64;
pub const MAX_VALUE_BYTES: usize = 60 * 1024;
pub const MAX_STORED_BYTES: u64 = 4 * 1024 * 1024;
pub const MAX_PAGE_BYTES: usize = 40 + MAX_KEYS * (10 + MAX_KEY_BYTES) + MAX_KEY_BYTES;
pub const MAX_CHUNK_BYTES: usize = 49 + MAX_VALUE_BYTES;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Root {
    pub level: u32,
    pub digest: [u8; 32],
}

pub fn root(present: u32, value: Root) -> Result<Option<Root>, Error> {
    match present {
        0 if value == Root::default() => Ok(None),
        1 if value.level <= u8::MAX as u32 => Ok(Some(value)),
        _ => Err(Error::InvalidArgument),
    }
}

#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct Query {
    pub size: u32,
    pub written: u32,
    pub root_level: u32,
    pub root_digest: [u8; 32],
    pub has_root: u32,
    pub limit: u32,
    pub reserved: u32,
}
#[derive(Clone, Copy, Debug)]
#[repr(C)]
pub struct Read {
    pub size: u32,
    pub written: u32,
    pub offset: u64,
    pub limit: u32,
    pub has_root: u32,
    pub root_level: u32,
    pub root_digest: [u8; 32],
    pub reserved: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Key<'a> {
    pub bytes: &'a [u8],
    pub len_bytes: u64,
}
fn key(bytes: &[u8]) -> Result<(), Error> {
    if bytes.is_empty() || bytes.len() > MAX_KEY_BYTES {
        Err(Error::InvalidArgument)
    } else {
        Ok(())
    }
}

fn page_shape<'a>(
    keys: impl Iterator<Item = Key<'a>>,
    next: Option<&[u8]>,
) -> Result<(u32, usize), Error> {
    let mut count = 0;
    let mut length = 40;
    let mut previous = None;
    for record in keys {
        key(record.bytes)?;
        if count == MAX_KEYS
            || record.len_bytes > MAX_STORED_BYTES
            || previous.is_some_and(|old| old >= record.bytes)
        {
            return Err(Error::InvalidArgument);
        }
        count += 1;
        length += 10 + record.bytes.len();
        previous = Some(record.bytes);
    }
    if let Some(next) = next {
        key(next)?;
        if previous.is_some_and(|last| last > next) {
            return Err(Error::InvalidArgument);
        }
        length += next.len();
    }
    assert!(length <= MAX_PAGE_BYTES, "bounded keys and cursor bound the frame");
    Ok((count as u32, length))
}

pub fn page_bytes<'a>(
    keys: impl Iterator<Item = Key<'a>>,
    next: Option<&[u8]>,
) -> Result<usize, Error> {
    page_shape(keys, next).map(|(_, length)| length)
}

pub fn encode_page<'a>(
    output: &mut [u8],
    root: Option<Root>,
    keys: impl Iterator<Item = Key<'a>> + Clone,
    next: Option<&[u8]>,
) -> Result<usize, Error> {
    let (count, length) = page_shape(keys.clone(), next)?;
    if output.len() < length {
        return Err(Error::BufferTooSmall);
    }
    if root.is_none() && (count != 0 || next.is_some()) {
        return Err(Error::InvalidArgument);
    }
    let mut writer = Writer::new(output);
    writer.u8(u8::from(root.is_some()))?;
    write_root(&mut writer, root.unwrap_or_default())?;
    writer.u32(count)?;
    writer.put(&(next.map_or(0, <[u8]>::len) as u16).to_le_bytes())?;
    for record in keys {
        writer.put(&(record.bytes.len() as u16).to_le_bytes())?;
        writer.u64(record.len_bytes)?;
        writer.put(record.bytes)?;
    }
    writer.put(next.unwrap_or_default())?;
    assert_eq!(writer.position(), length, "frame shape matches its encoding");
    Ok(length)
}
fn write_root(writer: &mut Writer<'_>, root: Root) -> Result<(), Error> {
    if root.level > u8::MAX as u32 {
        return Err(Error::InvalidArgument);
    }
    writer.u8(root.level as u8)?;
    writer.put(&root.digest)
}
fn read_root(reader: &mut Reader<'_>) -> Result<Root, Error> {
    Ok(Root {
        level: u32::from(reader.u8()?),
        digest: reader.take(32)?.try_into().map_err(|_| Error::Internal)?,
    })
}
fn u16(reader: &mut Reader<'_>) -> Result<u16, Error> {
    Ok(u16::from_le_bytes(reader.take(2)?.try_into().map_err(|_| Error::Internal)?))
}
fn page_payload_fits(count: usize, next: usize, remaining: usize) -> bool {
    next <= MAX_KEY_BYTES && next <= remaining && count <= (remaining - next) / 11
}

#[derive(Clone, Debug)]
pub struct Keys<'a> {
    bytes: &'a [u8],
    remaining: usize,
}
impl<'a> Iterator for Keys<'a> {
    type Item = Key<'a>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            return None;
        }
        // decode_page validated every range and record before constructing us.
        let length =
            u16::from_le_bytes(self.bytes[..2].try_into().expect("validated key length")) as usize;
        let len_bytes =
            u64::from_le_bytes(self.bytes[2..10].try_into().expect("validated value length"));
        let bytes = &self.bytes[10..10 + length];
        self.bytes = &self.bytes[10 + length..];
        self.remaining -= 1;
        Some(Key { bytes, len_bytes })
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.remaining, Some(self.remaining))
    }
}
impl ExactSizeIterator for Keys<'_> {}

#[derive(Clone, Debug)]
pub struct Page<'a> {
    pub root: Option<Root>,
    pub keys: Keys<'a>,
    pub next: Option<&'a [u8]>,
}
// fuzz: abi_reader
pub fn decode_page(bytes: &[u8]) -> Result<Page<'_>, Error> {
    if bytes.len() > MAX_PAGE_BYTES {
        return Err(Error::LimitExceeded);
    }
    let mut reader = Reader::new(bytes)?;
    let present = reader.u8()?;
    let root = root(u32::from(present), read_root(&mut reader)?)?;
    let count = reader.count(MAX_KEYS as u32)? as usize;
    let next_length = u16(&mut reader)? as usize;
    let begin = reader.position();
    let remaining = bytes.len() - begin;
    // Each nonempty key needs two length bytes, eight value-length bytes and
    // at least one key byte. Reject impossible counts before iterating.
    if !page_payload_fits(count, next_length, remaining) {
        return Err(Error::InvalidArgument);
    }
    let mut previous = None;
    for _ in 0..count {
        let length = u16(&mut reader)? as usize;
        let len_bytes = reader.u64()?;
        let bytes = reader.take(length)?;
        key(bytes)?;
        if len_bytes > MAX_STORED_BYTES || previous.is_some_and(|old| old >= bytes) {
            return Err(Error::InvalidArgument);
        }
        previous = Some(bytes);
    }
    let end = reader.position();
    let next = if next_length == 0 {
        None
    } else {
        let next = reader.take(next_length)?;
        key(next)?;
        if previous.is_some_and(|last| last > next) {
            return Err(Error::InvalidArgument);
        }
        Some(next)
    };
    if root.is_none() && (count != 0 || next.is_some()) {
        return Err(Error::InvalidArgument);
    }
    reader.finish()?;
    Ok(Page { root, keys: Keys { bytes: &bytes[begin..end], remaining: count }, next })
}

#[derive(Clone, Copy, Debug)]
pub struct Chunk<'a> {
    pub root: Root,
    pub total_bytes: u64,
    pub offset: u64,
    pub bytes: &'a [u8],
}
pub fn encode_chunk(output: &mut [u8], chunk: Chunk<'_>) -> Result<usize, Error> {
    validate_chunk(chunk)?;
    let length = 49 + chunk.bytes.len();
    if output.len() < length {
        return Err(Error::BufferTooSmall);
    }
    let mut writer = Writer::new(output);
    write_root(&mut writer, chunk.root)?;
    writer.u64(chunk.total_bytes)?;
    writer.u64(chunk.offset)?;
    writer.put(chunk.bytes)?;
    assert_eq!(writer.position(), length, "bounded chunk frame");
    Ok(length)
}
// fuzz: abi_reader
pub fn decode_chunk(bytes: &[u8]) -> Result<Chunk<'_>, Error> {
    if bytes.len() > MAX_CHUNK_BYTES {
        return Err(Error::LimitExceeded);
    }
    let mut reader = Reader::new(bytes)?;
    let root = read_root(&mut reader)?;
    let total_bytes = reader.u64()?;
    let offset = reader.u64()?;
    let chunk = Chunk { root, total_bytes, offset, bytes: &bytes[reader.position()..] };
    validate_chunk(chunk)?;
    Ok(chunk)
}
fn validate_chunk(chunk: Chunk<'_>) -> Result<(), Error> {
    if chunk.root.level > u8::MAX as u32
        || chunk.total_bytes > MAX_STORED_BYTES
        || chunk.offset > chunk.total_bytes
        || chunk.bytes.len() > MAX_VALUE_BYTES
        || chunk.bytes.len() as u64 > chunk.total_bytes - chunk.offset
    {
        return Err(Error::InvalidArgument);
    }
    Ok(())
}

#[cfg(test)]
mod tests;

/// # Safety
/// Context and all borrowed/disjoint pointers obey playbit/sys/abi.h.
#[inline]
#[allow(unsafe_code, reason = "Typed lowering to the scalar KV syscall boundary.")]
pub unsafe fn pb_kv_keys_begin(
    context: *mut super::operations::Context,
    entry: *const super::data::EntryHandle,
    query: *const Query,
    after: *const u8,
    length: u32,
) -> i64 {
    // SAFETY: the caller owns the selected operation's live borrowed regions.
    unsafe {
        super::invocation::invoke(
            context,
            super::syscall::Opcode::KvKeysBegin as u32,
            [
                entry as usize as u64,
                query as usize as u64,
                after as usize as u64,
                u64::from(length),
                0,
                0,
            ],
        )
    }
}

/// # Safety
/// Context and all borrowed/disjoint pointers obey playbit/sys/abi.h.
#[inline]
#[allow(unsafe_code, reason = "Typed lowering to the scalar KV syscall boundary.")]
pub unsafe fn pb_kv_read_begin(
    context: *mut super::operations::Context,
    entry: *const super::data::EntryHandle,
    key: *const u8,
    length: u32,
    query: *const Read,
) -> i64 {
    // SAFETY: the caller owns the selected operation's live borrowed regions.
    unsafe {
        super::invocation::invoke(
            context,
            super::syscall::Opcode::KvReadBegin as u32,
            [
                entry as usize as u64,
                key as usize as u64,
                u64::from(length),
                query as usize as u64,
                0,
                0,
            ],
        )
    }
}

/// # Safety
/// Context and all borrowed/disjoint pointers obey playbit/sys/abi.h.
#[inline]
#[allow(unsafe_code, reason = "Typed lowering to the scalar KV syscall boundary.")]
pub unsafe fn pb_kv_put_begin(
    context: *mut super::operations::Context,
    entry: *const super::data::EntryHandle,
    key: *const u8,
    length: u32,
    value: *const u8,
    value_length: u32,
) -> i64 {
    // SAFETY: the caller owns the selected operation's live borrowed regions.
    unsafe {
        super::invocation::invoke(
            context,
            super::syscall::Opcode::KvPutBegin as u32,
            [
                entry as usize as u64,
                key as usize as u64,
                u64::from(length),
                value as usize as u64,
                u64::from(value_length),
                0,
            ],
        )
    }
}

/// # Safety
/// Context and all borrowed/disjoint pointers obey playbit/sys/abi.h.
#[inline]
#[allow(unsafe_code, reason = "Typed lowering to the scalar KV syscall boundary.")]
pub unsafe fn pb_kv_delete_begin(
    context: *mut super::operations::Context,
    entry: *const super::data::EntryHandle,
    key: *const u8,
    length: u32,
) -> i64 {
    // SAFETY: the caller owns the selected operation's live borrowed regions.
    unsafe {
        super::invocation::invoke(
            context,
            super::syscall::Opcode::KvDeleteBegin as u32,
            [entry as usize as u64, key as usize as u64, u64::from(length), 0, 0, 0],
        )
    }
}

/// # Safety
/// Context and all borrowed/disjoint pointers obey playbit/sys/abi.h.
#[inline]
#[allow(unsafe_code, reason = "Typed lowering to the scalar KV syscall boundary.")]
pub unsafe fn pb_kv_keys_take(
    context: *mut super::operations::Context,
    handle: super::pending::OperationHandle,
    data: *mut u8,
    capacity: u32,
    count: *mut u32,
) -> i64 {
    // SAFETY: the caller owns the selected operation's live borrowed regions.
    unsafe {
        super::invocation::invoke(
            context,
            super::syscall::Opcode::KvKeysTake as u32,
            [handle.0, data as usize as u64, capacity as u64, count as usize as u64, 0, 0],
        )
    }
}

/// # Safety
/// Context and all borrowed/disjoint pointers obey playbit/sys/abi.h.
#[inline]
#[allow(unsafe_code, reason = "Typed lowering to the scalar KV syscall boundary.")]
pub unsafe fn pb_kv_read_take(
    context: *mut super::operations::Context,
    handle: super::pending::OperationHandle,
    data: *mut u8,
    capacity: u32,
    count: *mut u32,
) -> i64 {
    // SAFETY: the caller owns the selected operation's live borrowed regions.
    unsafe {
        super::invocation::invoke(
            context,
            super::syscall::Opcode::KvReadTake as u32,
            [handle.0, data as usize as u64, capacity as u64, count as usize as u64, 0, 0],
        )
    }
}

/// # Safety
/// Context and all borrowed/disjoint pointers obey playbit/sys/abi.h.
#[inline]
#[allow(unsafe_code, reason = "Typed lowering to the scalar KV syscall boundary.")]
pub unsafe fn pb_kv_put_take(
    context: *mut super::operations::Context,
    handle: super::pending::OperationHandle,
) -> i64 {
    // SAFETY: the caller owns the selected operation's live borrowed regions.
    unsafe {
        super::invocation::invoke(
            context,
            super::syscall::Opcode::KvPutTake as u32,
            [handle.0, 0, 0, 0, 0, 0],
        )
    }
}

/// # Safety
/// Context and all borrowed/disjoint pointers obey playbit/sys/abi.h.
#[inline]
#[allow(unsafe_code, reason = "Typed lowering to the scalar KV syscall boundary.")]
pub unsafe fn pb_kv_delete_take(
    context: *mut super::operations::Context,
    handle: super::pending::OperationHandle,
) -> i64 {
    // SAFETY: the caller owns the selected operation's live borrowed regions.
    unsafe {
        super::invocation::invoke(
            context,
            super::syscall::Opcode::KvDeleteTake as u32,
            [handle.0, 0, 0, 0, 0, 0],
        )
    }
}

impl Default for Query {
    fn default() -> Self {
        Self {
            size: core::mem::size_of::<Self>() as u32,
            written: 0,
            root_level: 0,
            root_digest: [0; 32],
            has_root: 0,
            limit: 0,
            reserved: 0,
        }
    }
}

super::record::record!(Query, 56, { root_level: u32, root_digest: [u8; 32], has_root: u32, limit: u32, reserved: u32 });

impl Default for Read {
    fn default() -> Self {
        Self {
            size: core::mem::size_of::<Self>() as u32,
            written: 0,
            offset: 0,
            limit: 0,
            has_root: 0,
            root_level: 0,
            root_digest: [0; 32],
            reserved: 0,
        }
    }
}

super::record::record!(Read, 64, { offset: u64, limit: u32, has_root: u32, root_level: u32, root_digest: [u8; 32], reserved: u32 });
