//! Structural checks only. Native allocation validity remains the C contract.
use super::{Error, Result};
#[cfg(feature = "native-runtime")]
pub(crate) use playbit_api::abi::record::read_native;
pub(crate) use playbit_api::abi::record::{clear_native, native_region, write_native};
use std::{
    mem::{align_of, size_of},
    ops::Range,
};

// These five public lifecycle records share the same explicit field inventory
// as syscall records. Fixed minimum sizes are frozen by the C/Rust probes.
macro_rules! record {
    ($ty:ty, $minimum:expr, {$($field:ident: $kind:ty),* $(,)?}) => {
        const _: fn(&$ty) = |record| { $(let _: &$kind = &record.$field;)* };
        impl playbit_api::abi::record::Record for $ty {
            const MIN_SIZE: u32 = $minimum;
            const FIELDS: &'static [playbit_api::abi::record::Field] = &[
                playbit_api::abi::record::Field { name: "size", kind: "u32", offset: 0, width: 4 },
                playbit_api::abi::record::Field { name: "written", kind: "u32", offset: 4, width: 4 },
                $(playbit_api::abi::record::Field {
                    name: stringify!($field), kind: stringify!($kind),
                    offset: core::mem::offset_of!($ty, $field) as u32,
                    width: core::mem::size_of::<$kind>() as u32,
                }),*
            ];
        }
    };
}
pub(super) use record;

fn region(start: usize, length: usize, alignment: usize) -> Result<Range<usize>> {
    if start == 0 || !start.is_multiple_of(alignment) || length > isize::MAX as usize {
        return Err(Error::InvalidArgument);
    }
    Ok(start..start.checked_add(length).ok_or(Error::InvalidArgument)?)
}
pub(crate) fn fixed<T>(pointer: *const T) -> Result<Range<usize>> {
    region(pointer as usize, size_of::<T>(), align_of::<T>())
}
pub(crate) fn input(pointer: *const u8, length: u32, limit: u32) -> Result<Range<usize>> {
    if length > limit {
        return Err(Error::LimitExceeded);
    }
    if length == 0 {
        return Err(Error::InvalidArgument);
    }
    region(pointer as usize, length as usize, 1)
}
pub(super) fn disjoint(a: &Range<usize>, b: &Range<usize>) -> Result<()> {
    if a.start < b.end && b.start < a.end { Err(Error::InvalidArgument) } else { Ok(()) }
}
