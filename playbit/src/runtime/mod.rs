//! App execution, bounded instance state and semantic presentation.
//!
//! Guests import the shared scalar Playbit syscall entries and receive no WASI imports or ambient
//! filesystem, network, environment, or clock access. Host operations use the
//! current volume/object capabilities and generation-scoped event contract.
//! The wasm module owns VM configuration and lifecycle; view owns bounded
//! scene decoding. Shared syscall operations own no VM. EventPoll and VM fuel
//! accounting are data plane; neither owns a second mutable scene.

#![deny(clippy::unwrap_used)]
#[cfg(all(feature = "native-audio", target_vendor = "apple"))]
pub mod audio;
#[cfg(all(feature = "native-audio", target_vendor = "apple"))]
mod audio_handles;
pub mod capabilities;
pub mod file_handles;
pub mod file_list;
pub mod host;
#[cfg(any(feature = "wasm-runtime", feature = "native-runtime"))]
mod instance;
#[cfg(any(test, target_arch = "wasm32", feature = "test-support", feature = "wasm-runtime"))]
pub(crate) mod linear_memory;
#[cfg(feature = "wasm-runtime")]
pub mod managed;
#[cfg(feature = "native-runtime")]
pub mod native;
#[cfg(any(feature = "wasm-runtime", feature = "native-runtime"))]
pub mod syscall;
mod view;
#[cfg(feature = "wasm-runtime")]
mod wasm;
#[cfg(any(feature = "wasm-runtime", feature = "native-runtime", target_arch = "wasm32"))]
pub(crate) mod work;
#[cfg(all(feature = "wasm-runtime", feature = "local-store"))]
pub(crate) use wasm::compiled;
#[cfg(feature = "wasm-runtime")]
pub use wasm::{ExecutionBackend, Runtime};

pub use view::{
    AutocompleteEntry, CanvasItem, MenuEntry, MenuGlyph, SegmentEntry, ViewNode, decode_view,
};

use std::error::Error;
use std::fmt;

use playbit_api::abi::syscall::{MAIN_EXPORT, MEMORY_EXPORT};

// Leave room for a maximum call buffer and its guest-owned source data.
pub const DEFAULT_MEMORY_LIMIT: usize = 128 * 1024 * 1024;
#[cfg(any(test, feature = "test-support"))]
const TEST_FUEL: u64 = 100_000_000;
const _: () = assert!(DEFAULT_MEMORY_LIMIT.is_multiple_of(65536));
const _: () = assert!(playbit_api::abi::syscall::MAX_CALL_BYTES <= DEFAULT_MEMORY_LIMIT);
/// Output produced by one guest entry-point invocation.
#[derive(Debug, Eq, PartialEq)]
pub struct Frame {
    logs: Vec<String>,
    view: Option<ViewNode>,
    view_bytes: Option<Vec<u8>>,
    quit_on_last_window: bool,
}

impl Frame {
    pub fn quit_on_last_window(&self) -> bool {
        self.quit_on_last_window
    }
    pub fn logs(&self) -> &[String] {
        &self.logs
    }

    pub fn view(&self) -> Option<&ViewNode> {
        self.view.as_ref()
    }

    /// Original validated scene encoding, suitable for a private process boundary.
    pub fn view_bytes(&self) -> Option<&[u8]> {
        self.view_bytes.as_deref()
    }

    /// Move logs and the decoded view into a native renderer without cloning.
    /// Drops the encoded copy used by hosts with a separate presentation process.
    pub fn into_native_parts(self) -> (Vec<String>, Option<ViewNode>) {
        (self.logs, self.view)
    }

    /// A local owner may retain the validated encoding for explicit tool
    /// inspection. It remains independent of the renderer and shared engine.
    pub fn into_parts(self) -> (Vec<String>, Option<ViewNode>, Option<Vec<u8>>) {
        (self.logs, self.view, self.view_bytes)
    }

    pub fn into_view(self) -> Option<ViewNode> {
        self.view
    }
}

#[derive(Debug)]
pub enum RuntimeError {
    Engine(String),
    InvalidModule(String),
    UnsupportedStartFunction,
    Linker(String),
    Instantiation(String),
    MissingMemory,
    InvalidMemoryExport,
    MissingMain,
    InvalidMainSignature(String),
    FuelExhausted,
    GuestTrap(String),
    GuestHalted,
    NativeApp(i32),
    InvalidView(String),
}

impl fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Engine(message) => write!(f, "failed to configure Wasmtime: {message}"),
            Self::InvalidModule(message) => write!(f, "invalid WebAssembly module: {message}"),
            Self::UnsupportedStartFunction => write!(
                f,
                "guest must initialize through `{MAIN_EXPORT}`, not a WebAssembly start function"
            ),
            Self::Linker(message) => write!(f, "failed to configure host imports: {message}"),
            Self::Instantiation(message) => write!(f, "failed to instantiate guest: {message}"),
            Self::MissingMemory => write!(f, "guest does not export memory as `{MEMORY_EXPORT}`"),
            Self::InvalidMemoryExport => {
                write!(f, "guest export `{MEMORY_EXPORT}` is not a WebAssembly memory")
            }
            Self::MissingMain => write!(f, "guest does not export `{MAIN_EXPORT}`"),
            Self::InvalidMainSignature(message) => {
                write!(f, "guest export `{MAIN_EXPORT}` must have type () -> (): {message}")
            }
            Self::FuelExhausted => write!(f, "guest exhausted its computation budget"),
            Self::GuestTrap(message) => write!(f, "guest trapped: {message}"),
            Self::NativeApp(status) => write!(f, "native app returned failure status {status}"),
            Self::GuestHalted => write!(f, "guest is halted; reload before running it again"),
            Self::InvalidView(message) => write!(f, "host cannot render guest view: {message}"),
        }
    }
}

impl Error for RuntimeError {}

#[cfg(any(
    target_arch = "wasm32",
    feature = "connected-engine",
    all(feature = "local-store", any(feature = "wasm-runtime", feature = "native-runtime"))
))]
pub mod store_bridge;

#[cfg(all(test, unix, feature = "native-runtime", feature = "wasm-runtime"))]
mod tracing_tests;
