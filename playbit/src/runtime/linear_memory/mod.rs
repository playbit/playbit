//! Safe wasm32 record admission. The executor lends exclusive, nonshared memory
//! for one synchronous call; no guest references or output regions escape it.
use super::host::HostResult;
use playbit_api::abi::syscall::Error as HostError;

#[cfg(all(not(target_arch = "wasm32"), any(feature = "wasm-runtime", feature = "native-runtime")))]
pub(crate) mod authenticated;
pub(crate) mod input;
pub(crate) mod layout;
#[cfg(all(
    not(target_arch = "wasm32"),
    any(test, feature = "test-support", feature = "wasm-runtime")
))]
pub(crate) mod output;
pub(crate) mod record;
pub(crate) mod schema;
#[cfg(test)]
mod tests;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Region {
    start: usize,
    end: usize,
}
impl Region {
    pub(crate) fn bytes(self, memory: &[u8]) -> &[u8] {
        &memory[self.start..self.end]
    }
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn bytes_mut(self, memory: &mut [u8]) -> &mut [u8] {
        &mut memory[self.start..self.end]
    }
    pub(crate) fn len(self) -> usize {
        self.end - self.start
    }
    pub(crate) fn sub(self, offset: usize, length: usize) -> Self {
        assert!(offset <= self.len() && length <= self.len() - offset);
        Self { start: self.start + offset, end: self.start + offset + length }
    }
    pub(crate) fn read<const N: usize>(self, memory: &[u8], offset: usize) -> [u8; N] {
        self.sub(offset, N).bytes(memory).try_into().expect("fixed-width record field")
    }
    pub(crate) fn u32(self, memory: &[u8], offset: usize) -> u32 {
        u32::from_le_bytes(self.field(memory, offset))
    }
    pub(crate) fn u64(self, memory: &[u8], offset: usize) -> u64 {
        u64::from_le_bytes(self.field(memory, offset))
    }
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn write(self, memory: &mut [u8], offset: usize, value: &[u8]) {
        let destination = self.sub(offset, value.len());
        memory[destination.start..destination.end].copy_from_slice(value);
    }
    pub(crate) fn overlaps(self, other: Self) -> bool {
        self.start != self.end
            && other.start != other.end
            && self.start < other.end
            && other.start < self.end
    }
}

fn span(
    memory_length: usize,
    pointer: u32,
    count: u32,
    stride: u32,
    alignment: u32,
    limit: u32,
) -> HostResult<Region> {
    assert!(stride != 0 && alignment.is_power_of_two());
    if count > limit {
        return Err(HostError::LimitExceeded);
    }
    // Empty spans never dereference the pointer, matching the native ABI.
    if count == 0 {
        return Ok(Region { start: 0, end: 0 });
    }
    let end = u64::from(pointer) + u64::from(count) * u64::from(stride);
    if pointer == 0 || !pointer.is_multiple_of(alignment) || end > memory_length as u64 {
        return Err(HostError::InvalidArgument);
    }
    Ok(Region { start: pointer as usize, end: end as usize })
}

pub(crate) fn disjoint(regions: &[Region]) -> HostResult<()> {
    assert!(regions.len() <= 8, "fixed operation region bound");
    for (index, region) in regions.iter().enumerate() {
        if regions[index + 1..].iter().any(|other| region.overlaps(*other)) {
            return Err(HostError::InvalidArgument);
        }
    }
    Ok(())
}

pub(crate) struct Memory<'a> {
    pub(crate) data: &'a mut [u8],
    pub(crate) fuel: Option<u64>,
}
impl Memory<'_> {
    pub(crate) fn admit(
        &mut self,
        pointer: u32,
        count: u32,
        stride: u32,
        alignment: u32,
        limit: u32,
    ) -> HostResult<Region> {
        let region = span(self.data.len(), pointer, count, stride, alignment, limit)?;
        // Charge each admitted region, including nested or repeated input spans,
        // before decoding/copying it. Output capacity is charged before effects.
        let charge = (region.len() as u64).div_ceil(16);
        if let Some(fuel) = &mut self.fuel {
            if charge >= *fuel {
                *fuel = 0;
                return Err(HostError::LimitExceeded);
            }
            *fuel -= charge;
        }
        Ok(region)
    }
    pub(crate) fn input(
        &mut self,
        pointer: u32,
        count: u32,
        stride: u32,
        alignment: u32,
        limit: u32,
        output: Option<Region>,
    ) -> HostResult<Region> {
        let region = self.admit(pointer, count, stride, alignment, limit)?;
        if let Some(output) = output {
            disjoint(&[region, output])?;
        }
        Ok(region)
    }
}
