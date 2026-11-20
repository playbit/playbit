//! Caller-owned wasm32 output arrays. The outer descriptor and all arrays must
//! be pairwise disjoint before counting, fetching or publishing any result.
use super::record::Layout;
use super::{Memory, Region};
use crate::runtime::syscall::HostResult;

pub(in crate::runtime) struct Buffer {
    descriptor: Region,
    pub(in crate::runtime) data: Region,
    pub(in crate::runtime) capacity: u32,
    pub(in crate::runtime) stride: u32,
    layout: Option<Layout>,
}
impl Buffer {
    pub(in crate::runtime) fn admit(
        memory: &mut Memory<'_>,
        descriptor: Region,
        stride: u32,
        alignment: u32,
        limit: u32,
    ) -> HostResult<Self> {
        let pointer = descriptor.u32(memory.data, 0);
        let capacity = descriptor.u32(memory.data, 4);
        let data = memory.admit(pointer, capacity, stride, alignment, limit)?;
        Ok(Self { descriptor, data, capacity, stride, layout: None })
    }
    pub(in crate::runtime) fn records(
        memory: &mut Memory<'_>,
        descriptor: Region,
        layout: Layout,
        limit: u32,
    ) -> HostResult<Self> {
        let pointer = descriptor.u32(memory.data, 0);
        let capacity = descriptor.u32(memory.data, 4);
        let stride = descriptor.u32(memory.data, 12);
        let data = memory.record_array(pointer, capacity, stride, layout, false, limit, None)?;
        Ok(Self { descriptor, data, capacity, stride, layout: Some(layout) })
    }
    pub(in crate::runtime) fn element(&self, memory: &[u8], index: usize) -> Region {
        assert!(index < self.capacity as usize, "output row fits admitted capacity");
        self.data.slot(memory, index, self.stride)
    }
    pub(in crate::runtime) fn clear(&self, memory: &mut [u8]) {
        if self.layout.is_some() {
            for index in 0..self.capacity as usize {
                self.element(memory, index).clear(memory);
            }
        }
    }
    pub(in crate::runtime) fn publish(&self, memory: &mut [u8], index: usize) {
        self.element(memory, index).publish(memory, self.layout.expect("record array layout"));
    }
    pub(in crate::runtime) fn count(&self, memory: &mut [u8], count: usize) {
        let count = u32::try_from(count).expect("bounded host result count");
        self.descriptor.write(memory, 8, &count.to_le_bytes());
    }
    #[cfg(any(test, feature = "wasm-runtime"))]
    pub(in crate::runtime) fn fits(&self, count: usize) -> bool {
        count <= self.capacity as usize
    }
    #[cfg(any(test, feature = "wasm-runtime"))]
    pub(in crate::runtime) fn append(
        &self,
        memory: &mut [u8],
        used: &mut usize,
        bytes: &[u8],
    ) -> [u8; 8] {
        let mut range = [0; 8];
        range[..4].copy_from_slice(&(*used as u32).to_le_bytes());
        range[4..].copy_from_slice(&(bytes.len() as u32).to_le_bytes());
        self.data.write(memory, *used, bytes);
        *used += bytes.len();
        range
    }
}
