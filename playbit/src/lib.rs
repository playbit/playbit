#![allow(unexpected_cfgs, reason = "objc macros check their optional cargo-clippy feature.")]
//! Everything that runs on a user's device: the local store and runtime, the
//! local `playbit-engine` engine and its AP client, the platform entry points,
//! and the macOS application. The remote AP service lives in `../ap`.
//!
//! The web client uses package/store services and the shared typed runtime
//! contract and memory validators. Native execution and I/O are feature-gated.

pub mod capability;
pub mod examples;
pub mod identifier;
pub mod package;
pub mod platform;
pub mod release_channel;
#[cfg_attr(
    not(any(target_arch = "wasm32", feature = "local-store")),
    allow(
        dead_code,
        reason = "Display-only package builds use shared schemas without a storage host."
    )
)]
pub mod store;
/// Client release and source revision, resolved at build time.
pub const VERSION: &str = env!("PLAYBIT_BUILD_VERSION");
/// SemVer release identity, without the source revision added to VERSION.
pub const RELEASE_VERSION: &str = env!("CARGO_PKG_VERSION");
/// Monotonic distribution build; zero identifies an unpublished development build.
pub const RELEASE_SEQUENCE: u64 = include!(concat!(env!("OUT_DIR"), "/release_sequence.rs"));
/// Tracing marks and spans. A separate crate (`playbit/trace`) because the
/// vendored gpui instruments itself with it.
pub use playbit_trace as trace;
pub use playbit_trace::diagnostic;

#[cfg(all(not(target_arch = "wasm32"), feature = "process"))]
pub mod client;
#[cfg(all(
    target_vendor = "apple",
    feature = "local-store",
    any(test, feature = "connected-engine")
))]
mod credentials;
#[cfg(all(not(target_arch = "wasm32"), feature = "local-store"))]
pub mod engine;
#[cfg(all(not(target_arch = "wasm32"), feature = "ap-protocol"))]
pub mod network;
#[cfg(all(not(target_arch = "wasm32"), feature = "local-store"))]
pub mod paths;
#[cfg(any(
    target_arch = "wasm32",
    feature = "presentation",
    feature = "wasm-runtime",
    feature = "native-runtime",
    feature = "connected-engine",
    feature = "desktop-ui"
))]
pub mod runtime;
#[cfg(all(not(target_arch = "wasm32"), any(feature = "native-ui", feature = "app-host")))]
pub mod ui;

#[cfg(feature = "desktop")]
pub mod desktop;
#[cfg(feature = "desktop")]
mod secrets;

pub mod session_channel;
