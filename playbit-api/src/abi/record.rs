//! Addressed ABI records have a capacity and an independently reported extent.
//! Layout declarations are shared by admission and frozen compiler probes.
#![allow(unsafe_code, reason = "Native record admission copies only admitted complete C fields.")]
use super::syscall::Error;
use core::{
    mem::{MaybeUninit, align_of, size_of},
    ops::Range,
};

pub const MAX_RECORD_BYTES: u32 = 4096;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Field {
    pub name: &'static str,
    pub kind: &'static str,
    pub offset: u32,
    pub width: u32,
}

pub trait Record: Copy {
    /// Oldest complete published allocation, including its tail padding.
    const MIN_SIZE: u32;
    const FIELDS: &'static [Field];
}

/// SDK storage initialization. Byte arrays are scalar spans, not record arrays.
pub trait Element: Copy {
    const RECORD: bool;
    const MIN_SIZE: u32;
    const FIELDS: &'static [Field];
    fn initialize(&mut self);
}
impl Element for u8 {
    const RECORD: bool = false;
    const MIN_SIZE: u32 = 1;
    const FIELDS: &'static [Field] = &[];
    fn initialize(&mut self) {}
}

/// Validate an allocation without inspecting implicit padding. Unknown input
/// bytes are checked by the memory adapter after admitting the claimed region.
pub fn admit<T: Record>(size: u32, output: bool) -> Result<(), Error> {
    if size > MAX_RECORD_BYTES {
        return Err(Error::LimitExceeded);
    }
    if size < T::MIN_SIZE {
        return Err(if output { Error::BufferTooSmall } else { Error::InvalidArgument });
    }
    for field in T::FIELDS {
        let end = field.offset + field.width;
        if field.offset < size && size < end {
            return Err(Error::InvalidArgument);
        }
    }
    Ok(())
}

/// Complete supported fields; capacity alone does not initialize a future tail.
pub fn extent<T: Record>(size: u32) -> u32 {
    T::FIELDS
        .iter()
        .filter_map(|field| {
            let end = field.offset + field.width;
            (end <= size).then_some(end)
        })
        .max()
        .unwrap_or(0)
}

/// Admit only the size header before inspecting the claimed allocation.
/// # Safety
/// The aligned pointer lends four readable bytes and, after structural admission,
/// its entire declared allocation. Native allocation validity belongs to the caller.
pub unsafe fn native_region<T: Record>(
    pointer: *const T,
    output: bool,
) -> Result<Range<usize>, Error> {
    let start = pointer as usize;
    if start == 0 || !start.is_multiple_of(align_of::<T>()) || start.checked_add(4).is_none() {
        return Err(Error::InvalidArgument);
    }
    // SAFETY: only the caller's four-byte header is read before allocation admission.
    let size = unsafe { pointer.cast::<u32>().read() };
    admit::<T>(size, output)?;
    Ok(start..start.checked_add(size as usize).ok_or(Error::InvalidArgument)?)
}

/// Copy complete fields into zero defaults without reading implicit padding.
/// # Safety
/// The caller lends the declared readable allocation. T must contain only
/// zero-valid integer, byte-array, pointer or nullable callback fields described
/// by Record::FIELDS. Copied pointer/callback representations must be valid.
pub unsafe fn read_native<T: Record>(pointer: *const T, input: bool) -> Result<T, Error> {
    // SAFETY: caller guarantees the native allocation for the admitted span.
    let capacity = unsafe { native_region(pointer, !input)? }.len();
    let source = pointer.cast::<u8>();
    if input && capacity > size_of::<T>() {
        // SAFETY: extension bytes are within the bounded caller allocation.
        let tail = unsafe {
            core::slice::from_raw_parts(source.add(size_of::<T>()), capacity - size_of::<T>())
        };
        if tail.iter().any(|byte| *byte != 0) {
            return Err(Error::Unsupported);
        }
    }
    let mut value = MaybeUninit::<T>::zeroed();
    for field in T::FIELDS {
        if (field.offset + field.width) as usize <= capacity {
            // SAFETY: this complete declared field exists in both allocations;
            // source and local destination do not overlap, and padding is excluded.
            unsafe {
                core::ptr::copy_nonoverlapping(
                    source.add(field.offset as usize),
                    value.as_mut_ptr().cast::<u8>().add(field.offset as usize),
                    field.width as usize,
                );
            }
        }
    }
    // SAFETY: the caller guarantees zero-valid record fields and valid input representations.
    Ok(unsafe { value.assume_init() })
}

/// Mark an admitted disjoint output as not yet initialized.
/// # Safety
/// The caller lends a writable admitted record and has checked disjointness with
/// all borrowed inputs. This must happen before work that can fail or consume state.
pub unsafe fn clear_native<T: Record>(pointer: *mut T) -> Result<(), Error> {
    // SAFETY: caller lends the stable declared allocation through this operation.
    unsafe {
        native_region(pointer, true)?;
    }
    // SAFETY: all output records include the size/written prefix.
    unsafe {
        pointer.cast::<u32>().add(1).write(0);
    }
    Ok(())
}

/// Publish complete supported fields, preserving capacity and unknown tail bytes.
/// # Safety
/// The caller lends writable disjoint storage admitted before effects. The size
/// header stays stable, and T's declared fields all have valid initialized values.
pub unsafe fn write_native<T: Record>(pointer: *mut T, value: T) -> Result<(), Error> {
    // SAFETY: the caller keeps the admitted allocation and header stable.
    let capacity = unsafe { native_region(pointer, true)? }.len() as u32;
    let source = (&value as *const T).cast::<u8>();
    for field in T::FIELDS.iter().skip(2) {
        if field.offset + field.width <= capacity {
            // SAFETY: complete field, disjoint local source, and no padding copied.
            unsafe {
                core::ptr::copy_nonoverlapping(
                    source.add(field.offset as usize),
                    pointer.cast::<u8>().add(field.offset as usize),
                    field.width as usize,
                );
            }
        }
    }
    // SAFETY: the prefix fits, and publication occurs only after all data fields.
    unsafe {
        pointer.cast::<u32>().add(1).write(extent::<T>(capacity));
    }
    Ok(())
}

macro_rules! record {
    ($ty:ty, $minimum:expr, {$($name:ident: $kind:ty),* $(,)?}) => {
        const _: fn(&$ty) = |record| {
            $(let _: &$kind = &record.$name;)*
        };
        impl $crate::abi::record::Record for $ty {
            const MIN_SIZE: u32 = $minimum;
            const FIELDS: &'static [$crate::abi::record::Field] = &[
                $crate::abi::record::Field { name: "size", kind: "u32", offset: 0, width: 4 },
                $crate::abi::record::Field { name: "written", kind: "u32", offset: 4, width: 4 },
                $($crate::abi::record::Field {
                    name: stringify!($name), kind: stringify!($kind),
                    offset: core::mem::offset_of!($ty, $name) as u32,
                    width: core::mem::size_of::<$kind>() as u32,
                }),*
            ];
        }
        impl $crate::abi::record::Element for $ty {
            const RECORD: bool = true;
            const MIN_SIZE: u32 = <$ty as $crate::abi::record::Record>::MIN_SIZE;
            const FIELDS: &'static [$crate::abi::record::Field] =
                <$ty as $crate::abi::record::Record>::FIELDS;
            fn initialize(&mut self) {
                self.size = core::mem::size_of::<Self>() as u32;
                self.written = 0;
            }
        }
    };
}
pub(crate) use record;

#[cfg(test)]
mod tests {
    use super::*;
    #[derive(Clone, Copy)]
    #[repr(C)]
    struct Extended {
        size: u32,
        written: u32,
        base: u32,
        added: u32,
    }
    record!(Extended, 12, { base: u32, added: u32 });

    #[test]
    fn historical_prefixes_and_complete_fields_define_extent() {
        // An older allocation remains valid; an interior-field cut is malformed.
        assert_eq!(admit::<Extended>(12, false), Ok(()));
        assert_eq!(extent::<Extended>(12), 12);
        assert_eq!(extent::<Extended>(32), 16);
        assert_eq!(admit::<Extended>(14, false), Err(Error::InvalidArgument));
        assert_eq!(admit::<Extended>(8, true), Err(Error::BufferTooSmall));
        assert_eq!(admit::<Extended>(8, false), Err(Error::InvalidArgument));
        assert_eq!(admit::<Extended>(MAX_RECORD_BYTES + 1, false), Err(Error::LimitExceeded));
    }

    #[test]
    fn native_short_allocations_and_future_tails_obey_capacity() {
        #[derive(Clone, Copy)]
        #[repr(C)]
        struct Prefix {
            size: u32,
            written: u32,
            base: u32,
        }
        #[repr(C)]
        struct Future {
            value: Extended,
            tail: [u8; 8],
        }
        let mut prefix = Prefix { size: 12, written: 12, base: 7 };
        let mut future =
            Future { value: Extended { size: 24, written: 0, base: 9, added: 11 }, tail: [0; 8] };
        // SAFETY: each pointer lends exactly its declared concrete allocation;
        // the historical prefix is deliberately smaller than Extended.
        unsafe {
            let pointer = (&mut prefix as *mut Prefix).cast::<Extended>();
            let copied = read_native(pointer, true).expect("short complete prefix");
            assert_eq!((copied.base, copied.added), (7, 0));
            clear_native(pointer).expect("clear admitted prefix");
            assert_eq!(prefix.written, 0);
            write_native(pointer, Extended { size: 16, written: 0, base: 13, added: 17 })
                .expect("write only fitting field");
            assert_eq!((prefix.size, prefix.written, prefix.base), (12, 12, 13));
            assert_eq!(read_native(&future.value, true).expect("zero extension").added, 11);
            future.tail.fill(0xa5);
            assert!(matches!(read_native(&future.value, true), Err(Error::Unsupported)));
            clear_native(&mut future.value).expect("output tail is opaque");
            write_native(&mut future.value, copied).expect("publish current fields");
            assert_eq!((future.value.size, future.value.written), (24, 16));
            assert_eq!(future.tail, [0xa5; 8]);
        }
    }
}
