//! Typed syscall operations and caller-owned records shared by apps and hosts.
//! Volume and operation handles are scoped to one app generation.
//! Packed UI and event payloads use bounded slice codecs; operation arguments
//! use the C records below. Malformed inputs return explicit errors.
//! Cursor and wire-width assertions stay enabled in release builds.

#[cfg(not(target_endian = "little"))]
compile_error!("Playbit supports only little-endian targets");

mod descriptors;
pub mod dispatch;
pub mod record;
pub mod syscall;

pub mod audio;
pub mod operations;

pub mod bloblog;
pub mod data;
pub mod kv;

pub mod schema;

pub mod environment;

pub mod files;

pub mod pending;
pub mod session;

pub mod authenticated;

pub(crate) mod invocation;

pub mod input;
pub mod text;
pub mod trace;
