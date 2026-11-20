//! Host-selected query ceilings, fixed for one app instance. The host must admit
//! its own scratch/allocations; these limits bound requested rows and delivered
//! logical value payload, not total resident memory or result framing.
#[cfg(any(target_arch = "wasm32", feature = "native-runtime", feature = "wasm-runtime"))]
use super::{HostError, HostResult};
#[cfg(any(target_arch = "wasm32", feature = "native-runtime", feature = "wasm-runtime"))]
use playbit_api::abi::environment::HostLimit;
use playbit_api::abi::syscall;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct QueryLimits {
    pub records: u32,
    pub value_bytes: u32,
}
impl Default for QueryLimits {
    fn default() -> Self {
        Self { records: syscall::MAX_PAGE_OBJECTS, value_bytes: syscall::MAX_DATA_BYTES as u32 }
    }
}
#[cfg(any(target_arch = "wasm32", feature = "native-runtime", feature = "wasm-runtime"))]
impl QueryLimits {
    pub(crate) fn validate(self) -> HostResult<()> {
        if self.records == 0
            || self.records > syscall::MAX_PAGE_OBJECTS
            || self.value_bytes == 0
            || self.value_bytes > syscall::MAX_DATA_BYTES as u32
        {
            return Err(HostError::InvalidArgument);
        }
        Ok(())
    }
    pub(crate) fn get(self, kind: u32) -> HostResult<i32> {
        let value = match kind {
            x if x == HostLimit::QueryRecords as u32 => self.records,
            x if x == HostLimit::QueryValueBytes as u32 => self.value_bytes,
            _ => return Err(HostError::Unsupported),
        };
        self.validate()?;
        i32::try_from(value).map_err(|_| HostError::Internal)
    }
}
