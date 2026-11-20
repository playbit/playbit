// Native and Wasm ABI adapters opt out locally; all other SDK code denies unsafe.
#![deny(unsafe_code, clippy::unwrap_used)]

/// Experimental document database types, available with the default features.
#[cfg(feature = "docdb")]
pub use multi_rs as docdb;

pub mod abi;
pub mod ui;

pub mod context;

#[cfg(any(feature = "guest", test))]
mod app;
#[cfg(all(not(target_arch = "wasm32"), any(feature = "guest", test)))]
pub use app::native_app;
#[cfg(any(feature = "guest", test))]
pub use app::{App, app};
