//! Size-prefix admission for explicit, compiler-verified wasm32 layouts.
use super::{Memory, Region, disjoint};
use crate::runtime::host::HostResult;
use playbit_api::abi::{record::MAX_RECORD_BYTES, syscall::Error};

#[derive(Clone, Copy)]
pub(crate) struct Layout {
    pub minimum: u32,
    pub size: u32,
    pub alignment: u32,
    pub fields: &'static [(usize, usize)],
}
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn outputs(regions: &[Region]) -> HostResult<()> {
    let bytes = regions.iter().skip(1).map(|region| region.len()).sum::<usize>();
    if bytes > playbit_api::abi::syscall::MAX_QUERY_SCRATCH_BYTES {
        return Err(Error::LimitExceeded);
    }
    disjoint(regions)
}
impl Layout {
    /// Validate output structure using the fields preceding an all-or-nothing extension.
    /// The caller still admits the entire declared allocation and publishes with the full layout.
    pub(crate) const fn before_output_tail(self, offset: usize) -> Self {
        let Some((&(last, _), fields)) = self.fields.split_last() else {
            panic!("empty ABI layout")
        };
        assert!(last == offset, "expected the declared output tail");
        Self { fields, ..self }
    }

    fn admit(self, size: u32, input: bool) -> HostResult<()> {
        if size > MAX_RECORD_BYTES {
            return Err(Error::LimitExceeded);
        }
        if size < self.minimum {
            return Err(if input { Error::InvalidArgument } else { Error::BufferTooSmall });
        }
        for &(offset, width) in self.fields {
            if offset < size as usize && (size as usize) < offset + width {
                return Err(Error::InvalidArgument);
            }
        }
        Ok(())
    }
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn extent(self, capacity: usize) -> u32 {
        self.fields
            .iter()
            .filter_map(|&(offset, width)| {
                let end = offset + width;
                (end <= capacity).then_some(end as u32)
            })
            .max()
            .unwrap_or(0)
    }
}
impl Memory<'_> {
    pub(crate) fn record(
        &mut self,
        pointer: u32,
        layout: Layout,
        input: bool,
        excluded: Option<Region>,
    ) -> HostResult<Region> {
        let header = self.admit(pointer, 1, 4, layout.alignment, 1)?;
        let size = header.u32(self.data, 0);
        layout.admit(size, input)?;
        let record = self.admit(pointer, size, 1, layout.alignment, MAX_RECORD_BYTES)?;
        if let Some(excluded) = excluded {
            disjoint(&[record, excluded])?;
        }
        if input
            && size > layout.size
            && record
                .sub(layout.size as usize, (size - layout.size) as usize)
                .bytes(self.data)
                .iter()
                .any(|byte| *byte != 0)
        {
            return Err(Error::Unsupported);
        }
        Ok(record)
    }
    #[allow(
        clippy::too_many_arguments,
        reason = "Explicit independent ABI bounds and caller stride."
    )]
    pub(crate) fn record_array(
        &mut self,
        pointer: u32,
        count: u32,
        stride: u32,
        layout: Layout,
        input: bool,
        limit: u32,
        excluded: Option<Region>,
    ) -> HostResult<Region> {
        if stride > MAX_RECORD_BYTES {
            return Err(Error::LimitExceeded);
        }
        if stride < layout.minimum || !stride.is_multiple_of(layout.alignment) {
            return Err(Error::InvalidArgument);
        }
        let records = self.input(pointer, count, stride, layout.alignment, limit, excluded)?;
        if records.len() > playbit_api::abi::syscall::MAX_QUERY_SCRATCH_BYTES {
            return Err(Error::LimitExceeded);
        }
        for index in 0..count {
            let size = records.sub(index as usize * stride as usize, 4).u32(self.data, 0);
            if size > stride {
                return Err(Error::InvalidArgument);
            }
            let offset = pointer
                .checked_add(index.checked_mul(stride).ok_or(Error::InvalidArgument)?)
                .ok_or(Error::InvalidArgument)?;
            self.record(offset, layout, input, excluded)?;
        }
        Ok(records)
    }
}
impl Region {
    /// An appended input field absent from an admitted older prefix has its
    /// all-zero default. Structural admission rejects a partially present field.
    pub(crate) fn field<const N: usize>(self, memory: &[u8], offset: usize) -> [u8; N] {
        if offset >= self.len() {
            return [0; N];
        }
        self.read(memory, offset)
    }
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn initialize(self, memory: &mut [u8], layout: Layout) {
        self.clear(memory);
        for &(offset, width) in layout.fields.iter().skip(2) {
            if offset + width <= self.len() {
                self.sub(offset, width).bytes_mut(memory).fill(0);
            }
        }
    }
    pub(crate) fn slot(self, memory: &[u8], index: usize, stride: u32) -> Self {
        let slot = self.sub(index * stride as usize, stride as usize);
        let size = slot.u32(memory, 0) as usize;
        assert!(size <= stride as usize, "admitted record fits caller stride");
        slot.sub(0, size)
    }
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn clear(self, memory: &mut [u8]) {
        self.write(memory, 4, &0u32.to_le_bytes());
    }
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn publish(self, memory: &mut [u8], layout: Layout) {
        self.write(memory, 4, &layout.extent(self.len()).to_le_bytes());
    }
}
