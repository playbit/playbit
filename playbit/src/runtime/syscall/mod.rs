//! VM-independent, checked Playbit operations.
//!
//! Named native and Wasm adapters validate external memory before calling these
//! operations. Hosts bind a verified principal and recheck current authority;
//! caller IDs and mount labels never establish rights. State owns generations,
//! bounded queues, operation handles and the accepted semantic scene.

#[cfg(test)]
use std::collections::{BTreeMap, BTreeSet};
#[cfg(test)]
use std::time::{Duration, Instant};

pub use playbit_api::abi::data::EntryHandle;
use playbit_api::abi::syscall as abi;
pub use playbit_api::abi::syscall::{
    Error as HostError, EventKind, Id, Opcode, OperationHandle, VolumeHandle,
};
#[cfg(test)]
use playbit_api::abi::syscall::{Reader, Writer};

#[cfg(feature = "wasm-runtime")]
pub use crate::runtime::wasm::{GuestInstance, PreparedGuest};
#[cfg(test)]
use crate::runtime::{Runtime, RuntimeError, decode_view};

pub use crate::runtime::instance::{Event, PendingUpdate, validate_volumes};
use crate::runtime::instance::{
    MAX_QUEUED_EVENT_BYTES, MAX_QUEUED_EVENTS, PendingOperation, State,
};

pub use super::host::HostResult;

pub(in crate::runtime) fn require_supported(state: &State, opcode: Opcode) -> HostResult<()> {
    if state.supported.contains(&opcode) { Ok(()) } else { Err(HostError::Unsupported) }
}
const MAX_VALUES: u32 = abi::MAX_VALUES;
const MAX_SCHEMA_FIELDS: u32 = 4096;
const MAX_SCHEMA_TYPES: u32 = 256;

pub(super) mod audio;
pub(super) mod authenticated_field;
pub(super) mod channels;
pub(super) mod control;
pub(crate) mod data_validation;
pub(super) mod environment;
pub(super) mod file_ops;
pub(in crate::runtime) mod handles;
pub(super) mod pending_data;
pub(super) mod pending_files;
pub(super) mod tracing;
pub(super) use handles::resolve_volume;
use handles::{consume_operation, register_operation, reserve_operation};
pub(super) mod events;
pub use events::validate_input;
pub(super) use events::{push_event, queue_due_wake};
mod files;
pub(super) use files::file_descriptor;

pub use super::host::{
    AuthenticatedFieldRead, AvailableVolume, EntryAddress, FieldChange, FieldDeclaration,
    FieldProvenance, FieldRevision, FileChunk, FileCopyRequest, FileSelection, GuestContext, Host,
    Identity, ObjectPage, ObjectRecord, Participant, Principal, Schema, TypeDeclaration, Value,
    ValueType, VolumeRights,
};

/// Native presentation may fetch a bounded whole file; guest FileRead remains
/// chunked and bounded by the operation payload contract.
pub const MAX_PRESENTATION_FILE_BYTES: usize = 8 * 1024 * 1024;

#[cfg(feature = "test-support")]
#[path = "../fuzz/test_support.rs"]
pub mod fuzz;

#[cfg(test)]
mod channel_tests;
#[cfg(test)]
mod creation_tests;
#[cfg(test)]
mod file_copy_tests;

#[cfg(test)]
mod file_selection_tests;
#[cfg(test)]
mod test_support;

#[cfg(test)]
mod wake_tests;

#[cfg(test)]
mod diagnostic_tests;

#[cfg(test)]
mod tests;

#[cfg(test)]
mod data_tests;
