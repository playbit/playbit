//! One aggregate budget for the guest's linear memory and Wasmtime GC heap.
//! Wasmtime 50 sends both through the synchronous memory growth callback. The
//! copying collector's allocation includes both semispaces. Failed growth traps;
//! a failed store is never entered again, and its accounting is discarded.
use wasmtime::{ResourceLimiter, StoreLimits, StoreLimitsBuilder};

use crate::runtime::DEFAULT_MEMORY_LIMIT;

pub(super) struct GuestLimits {
    objects: StoreLimits,
    memory_bytes: usize,
    last_growth_bytes: usize,
}

impl GuestLimits {
    pub(super) fn new() -> Self {
        Self {
            objects: StoreLimitsBuilder::new()
                .memory_size(DEFAULT_MEMORY_LIMIT)
                .instances(1)
                .memories(1)
                .tables(1)
                .table_elements(10_000)
                .trap_on_grow_failure(true)
                .build(),
            memory_bytes: 0,
            last_growth_bytes: 0,
        }
    }
}

impl ResourceLimiter for GuestLimits {
    fn memory_growing(
        &mut self,
        current: usize,
        desired: usize,
        maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        self.last_growth_bytes = 0;
        self.objects.memory_growing(current, desired, maximum)?;
        let growth = desired.checked_sub(current).expect("memory growth cannot shrink");
        assert!(self.memory_bytes <= DEFAULT_MEMORY_LIMIT);
        if growth > DEFAULT_MEMORY_LIMIT - self.memory_bytes {
            return Err(wasmtime::Error::msg(
                "guest linear memory and GC heap exceed their shared budget",
            ));
        }
        self.memory_bytes += growth;
        self.last_growth_bytes = growth;
        Ok(true)
    }

    fn memory_grow_failed(&mut self, error: wasmtime::Error) -> wasmtime::Result<()> {
        // There is no success notification. Growth is synchronous and serialized;
        // an allocation failure immediately follows the corresponding admission.
        // Wasmtime can also report a type-overflow failure without admission. That
        // traps too, so accounting from such a store cannot grant more memory.
        self.memory_bytes -= self.last_growth_bytes;
        self.last_growth_bytes = 0;
        self.objects.memory_grow_failed(error)
    }

    fn table_growing(
        &mut self,
        current: usize,
        desired: usize,
        maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        self.objects.table_growing(current, desired, maximum)
    }

    fn table_grow_failed(&mut self, error: wasmtime::Error) -> wasmtime::Result<()> {
        self.objects.table_grow_failed(error)
    }

    fn instances(&self) -> usize {
        self.objects.instances()
    }

    fn tables(&self) -> usize {
        self.objects.tables()
    }

    fn memories(&self) -> usize {
        self.objects.memories()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aggregate_growth_and_allocation_failure() {
        // Two memories share the budget; failed permitted allocations restore it.
        let mut limits = GuestLimits::new();
        limits.memory_growing(0, 65536, None).expect("initial linear memory");
        limits.memory_growing(0, 1048576, None).expect("initial GC heap");
        let before = limits.memory_bytes;
        limits.memory_growing(1048576, 2097152, None).expect("heap growth");
        assert!(limits.memory_grow_failed(wasmtime::Error::msg("allocation failure")).is_err());
        assert_eq!(limits.memory_bytes, before);
        assert!(limits.memory_growing(65536, DEFAULT_MEMORY_LIMIT, None).is_err());
        assert_eq!(limits.memory_bytes, before);
        assert!(limits.memory_growing(65536, 131072, Some(65536)).is_err());
        assert_eq!(limits.memory_bytes, before);
    }
}
