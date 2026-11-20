//! Typed conversions between stored records and host operations. Callers own
//! authority and resource admission before any allocation or value copy here.
use playbit_api::abi::syscall::Error as HostError;

#[cfg(any(target_arch = "wasm32", feature = "native-runtime", feature = "wasm-runtime"))]
mod records;
#[cfg(all(
    not(target_arch = "wasm32"),
    any(feature = "native-runtime", feature = "wasm-runtime")
))]
pub(crate) use records::{database_address, sid};
#[cfg(all(
    not(target_arch = "wasm32"),
    any(feature = "native-runtime", feature = "wasm-runtime")
))]
pub use records::{from_owned_value, from_schema};
#[cfg(any(target_arch = "wasm32", feature = "native-runtime", feature = "wasm-runtime"))]
pub(crate) use records::{to_schema, to_value};

#[cfg(any(target_arch = "wasm32", feature = "native-runtime", feature = "wasm-runtime"))]
pub(crate) fn error(error: crate::store::Error) -> HostError {
    error_ref(&error)
}
pub(crate) fn error_ref(error: &crate::store::Error) -> HostError {
    use crate::store::Error;
    match error {
        Error::AccessDenied(_) | Error::ParticipantRevoked { .. } => HostError::AccessDenied,
        Error::Unsupported(_) => HostError::Unsupported,
        Error::NotFound(_) => HostError::NotFound,
        Error::AlreadyExists(_) | Error::LocalContention(_) => HostError::Conflict,
        Error::InvalidInput(_) => HostError::InvalidArgument,
        Error::LimitExceeded => HostError::LimitExceeded,
        _ => HostError::Internal,
    }
}
