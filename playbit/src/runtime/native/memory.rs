//! Structural native span admission. This proves arithmetic and alignment only;
//! allocation validity, aliasing and lifetime remain the native caller contract.
use crate::runtime::syscall::{HostError, HostResult};
use std::{
    mem::{align_of, size_of},
    ops::Range,
};

pub(super) fn span_at<T>(start: usize, count: usize) -> HostResult<Range<usize>> {
    let bytes = count
        .checked_mul(size_of::<T>())
        .filter(|n| *n <= isize::MAX as usize)
        .ok_or(HostError::InvalidArgument)?;
    if count != 0 && (start == 0 || !start.is_multiple_of(align_of::<T>())) {
        return Err(HostError::InvalidArgument);
    }
    let end = start.checked_add(bytes).ok_or(HostError::InvalidArgument)?;
    Ok(start..end)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn span_rejects_null_misalignment_length_and_address_overflow() {
        assert_eq!(span_at::<u64>(0, 0), Ok(0..0));
        assert_eq!(span_at::<u64>(3, 0), Ok(3..3));
        assert_eq!(span_at::<u64>(8, 2), Ok(8..24));
        for (address, count) in [(0, 1), (3, 1), (8, usize::MAX), (usize::MAX - 7, 1)] {
            assert_eq!(span_at::<u64>(address, count), Err(HostError::InvalidArgument));
        }
        assert_eq!(span_at::<u8>(1, isize::MAX as usize + 1), Err(HostError::InvalidArgument));
    }
}
