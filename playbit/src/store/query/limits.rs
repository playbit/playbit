//! Query-local accounting; backend caches and concurrent operations have separate budgets.
use crate::store::{Error, Result};
use playbit_api::abi::syscall::{MAX_DATA_BYTES, MAX_QUERY_SCRATCH_BYTES};

/// Host-chosen ceilings, bounded by the common ABI. They cannot expand app authority.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub output_bytes: usize,
    pub scratch_bytes: usize,
}
impl Limits {
    /// Existing desktop and web ceilings. Small devices choose smaller explicit limits.
    pub const ABI: Self =
        Self { output_bytes: MAX_DATA_BYTES, scratch_bytes: MAX_QUERY_SCRATCH_BYTES };

    pub(crate) fn start(self, records: usize, fields: usize) -> Result<Scratch> {
        if !(5..=MAX_DATA_BYTES).contains(&self.output_bytes)
            || self.scratch_bytes > MAX_QUERY_SCRATCH_BYTES
        {
            return Err(Error::LimitExceeded);
        }
        let framing = records
            .checked_mul(2 * core::mem::size_of::<super::ObjectFields>() + 32)
            .and_then(|n| fields.checked_mul(2 * core::mem::size_of::<super::Id>())?.checked_add(n))
            .and_then(|n| n.checked_add(2 * self.output_bytes))
            .ok_or(Error::LimitExceeded)?;
        let mut scratch = Scratch { used: 0, limit: self.scratch_bytes };
        scratch.charge(framing)?;
        Ok(scratch)
    }
}

pub(crate) struct Scratch {
    used: usize,
    limit: usize,
}
impl Scratch {
    pub(crate) fn check(&self, bytes: usize) -> Result<usize> {
        self.used.checked_add(bytes).filter(|n| *n <= self.limit).ok_or(Error::LimitExceeded)
    }
    pub(crate) fn charge(&mut self, bytes: usize) -> Result<()> {
        self.used = self.check(bytes)?;
        Ok(())
    }
    /// Charge before a backend can copy the raw bytes, then before typed decoding.
    pub(crate) fn raw(&self, length: usize, overhead: usize) -> Result<usize> {
        let charge = length
            .checked_mul(3)
            .and_then(|n| n.checked_add(overhead))
            .ok_or(Error::LimitExceeded)?;
        self.check(charge)?;
        Ok(charge)
    }
}
