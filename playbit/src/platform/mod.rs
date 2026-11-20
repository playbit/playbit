//! Platform entry points. The iOS static library and the web Wasm module are
//! built from this crate with `cargo rustc --crate-type staticlib` and
//! `--crate-type cdylib` respectively; see `../../platform/`.
pub mod failure;

#[cfg(all(target_os = "macos", feature = "quicklook"))]
pub mod quicklook;

// The UIKit bridge selects its complete composition explicitly.
#[cfg(all(not(target_arch = "wasm32"), feature = "ios"))]
pub mod ios;
#[cfg(target_arch = "wasm32")]
pub mod web;

#[cfg(all(not(target_arch = "wasm32"), feature = "file-provider"))]
pub mod provider;
