//! Guest trace wire definitions and little-endian encoding helpers.
use super::syscall::Error;

pub const HEADER_BYTES: usize = 40;
pub const MAX_BATCH_BYTES: usize = 64 * 1024;
pub const MAX_METADATA_BYTES: usize = 4096;
pub const BUFFER_BYTES: usize = 16 * 1024;
pub const START: u8 = 1;
pub const END: u8 = 2;
pub const INSTANT: u8 = 3;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TraceEvent {
    pub timestamp: u64,
    pub reference_eid: u64,
    pub value: u64,
    pub reference_tid: u32,
    pub metadata_len: u32,
    pub name: u16,
    pub kind: u8,
    pub reserved: [u8; 5],
}
const _: () = assert!(core::mem::size_of::<TraceEvent>() == HEADER_BYTES);
const _: () = assert!(core::mem::offset_of!(TraceEvent, reference_eid) == 8);
const _: () = assert!(core::mem::offset_of!(TraceEvent, reference_tid) == 24);
const _: () = assert!(core::mem::offset_of!(TraceEvent, metadata_len) == 28);
const _: () = assert!(core::mem::offset_of!(TraceEvent, kind) == 34);

pub fn make_timestamp(last_t: &mut u64, now_ns: u64) -> Option<u64> {
    let timestamp = now_ns.max(last_t.checked_add(1)?);
    *last_t = timestamp;
    Some(timestamp)
}

pub fn event_bytes(metadata_len: usize) -> Result<usize, Error> {
    if metadata_len > MAX_METADATA_BYTES {
        return Err(Error::LimitExceeded);
    }
    HEADER_BYTES
        .checked_add(metadata_len.checked_add(7).ok_or(Error::InvalidArgument)? & !7)
        .ok_or(Error::InvalidArgument)
}

#[allow(unsafe_code, reason = "The fixed C-layout header has no implicit padding.")]
pub fn encode_header(event: TraceEvent, output: &mut [u8]) -> Result<(), Error> {
    let output = output.get_mut(..HEADER_BYTES).ok_or(Error::InvalidArgument)?;
    // SAFETY: repr(C) and the layout checks establish that all 40 bytes are
    // initialized fields. The owned header cannot overlap the admitted output.
    unsafe {
        core::ptr::copy_nonoverlapping(
            (&raw const event).cast::<u8>(),
            output.as_mut_ptr(),
            HEADER_BYTES,
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn clock_samples_remain_unique_and_exhaustion_does_not_wrap() {
        // Repeated/backward samples stay unique; exhaustion preserves the last ID.
        let mut last = 0;
        for (now, expected) in [(0, 1), (20, 20), (20, 21), (19, 22)] {
            assert_eq!(make_timestamp(&mut last, now), Some(expected));
        }
        last = u64::MAX;
        assert_eq!(make_timestamp(&mut last, 0), None);
        assert_eq!(last, u64::MAX);
    }
}
