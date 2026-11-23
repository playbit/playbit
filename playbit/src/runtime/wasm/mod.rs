//! WebAssembly compilation and execution adapters.
//!
//! Owns VM configuration, code memory, import lowering, fuel and traps. App
//! state, service operations and semantic scene decoding live outside this
//! module so native execution can reuse them without a VM.

pub(crate) mod compiled;
mod deadline;
mod guest;
#[cfg(target_os = "macos")]
mod jit_memory;
mod limits;
mod operations;
mod wasm_atomics;
pub use guest::{GuestInstance, PreparedGuest};

use super::{DEFAULT_MEMORY_LIMIT, RuntimeError};
use std::num::NonZeroU64;
use wasmtime::{Config, Engine, Module};

/// The execution strategy used by a host. Both strategies use identical guest
/// imports, memory limits, and computation budgets.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExecutionBackend {
    Native,
    /// Compiles Wasm to interpreted bytecode, without executing generated
    /// native code or requiring executable guest memory.
    Pulley,
}

/// Wasmtime installs its macOS trap handling from the first `Engine::new` by
/// spawning a thread that waits in `mach_msg` with `MACH_RCV_INTERRUPT`. Any
/// Unix signal delivered to that thread makes the wait return
/// `MACH_RCV_INTERRUPTED`, which wasmtime treats as fatal and aborts the whole
/// process. SIGCHLD is delivered to an arbitrary thread as soon as anything in
/// the process installs a handler for it (tokio's process support does), so a
/// process that runs guests and reaps children this way aborts at random. A
/// new thread inherits the signal mask of the thread that creates it, so the
/// engine is created with every signal blocked: the handler thread can then
/// never be interrupted, and the mask is restored here before returning.
#[cfg(target_os = "macos")]
#[allow(unsafe_code, reason = "Native signal masks are restored by the owning guard.")]
fn with_signals_blocked<T>(f: impl FnOnce() -> T) -> T {
    // Darwin's `sigset_t` is a `u32`; these are its `<signal.h>` constants.
    type SigSet = u32;
    const SIG_BLOCK: i32 = 1;
    const SIG_SETMASK: i32 = 3;
    unsafe extern "C" {
        fn sigfillset(set: *mut SigSet) -> i32;
        fn pthread_sigmask(how: i32, set: *const SigSet, old: *mut SigSet) -> i32;
    }
    struct Restore(SigSet);
    impl Drop for Restore {
        fn drop(&mut self) {
            unsafe { pthread_sigmask(SIG_SETMASK, &self.0, std::ptr::null_mut()) };
        }
    }
    let mut all: SigSet = 0;
    let mut previous: SigSet = 0;
    let _restore = unsafe {
        sigfillset(&mut all);
        pthread_sigmask(SIG_BLOCK, &all, &mut previous);
        Restore(previous)
    };
    f()
}
#[cfg(not(target_os = "macos"))]
fn with_signals_blocked<T>(f: impl FnOnce() -> T) -> T {
    f()
}

#[derive(Clone)]
pub struct Runtime {
    engine: Engine,
    fuel_limit: Option<NonZeroU64>,
    cache: Option<std::sync::Arc<dyn compiled::Cache>>,
    compile_gate: std::sync::Arc<std::sync::Mutex<()>>,
    /// Owns the `MAP_JIT` mappings natively compiled guests run from, so the
    /// app needs only the `allow-jit` entitlement. See `jit_memory`.
    #[cfg(target_os = "macos")]
    jit: Option<std::sync::Arc<jit_memory::JitCodeMemory>>,
    #[cfg(test)]
    compilations: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl Runtime {
    pub fn new() -> Result<Self, RuntimeError> {
        Self::with_fuel_limit(None)
    }

    /// A positive local limit is reset before every invocation of `pb_main`.
    /// `None` disables Wasmtime fuel metering entirely.
    pub fn with_fuel_limit(fuel_limit: Option<NonZeroU64>) -> Result<Self, RuntimeError> {
        let backend = if cfg!(target_os = "ios") {
            ExecutionBackend::Pulley
        } else {
            ExecutionBackend::Native
        };
        Self::with_backend_and_fuel_limit(backend, fuel_limit)
    }

    pub fn with_backend(backend: ExecutionBackend) -> Result<Self, RuntimeError> {
        Self::with_backend_and_fuel_limit(backend, None)
    }

    pub fn fuel_limit(&self) -> Option<NonZeroU64> {
        self.fuel_limit
    }

    /// Preserve the cache's owner and compiler admission when selecting the
    /// local launch's metering mode. Positive values share compiled code.
    #[cfg(feature = "app-host")]
    pub(crate) fn for_fuel_limit(
        &self,
        fuel_limit: Option<NonZeroU64>,
    ) -> Result<Self, RuntimeError> {
        if self.fuel_limit.is_some() == fuel_limit.is_some() {
            let mut runtime = self.clone();
            runtime.fuel_limit = fuel_limit;
            return Ok(runtime);
        }
        let backend = if self.engine.is_pulley() {
            ExecutionBackend::Pulley
        } else {
            ExecutionBackend::Native
        };
        let mut runtime = Self::with_backend_and_fuel_limit(backend, fuel_limit)?;
        runtime.cache = self.cache.clone();
        runtime.compile_gate = self.compile_gate.clone();
        Ok(runtime)
    }

    pub fn with_backend_and_fuel_limit(
        backend: ExecutionBackend,
        fuel_limit: Option<NonZeroU64>,
    ) -> Result<Self, RuntimeError> {
        let mut config = Config::new();
        // Bundled guest debug information must work in GUI launches too, without
        // requiring an environment variable on the process that owns the engine.
        config.wasm_backtrace_details(wasmtime::WasmBacktraceDetails::Enable);
        config.consume_fuel(fuel_limit.is_some());
        config.wasm_bulk_memory(true);
        config.wasm_simd(true);
        config.wasm_gc(true);
        config.wasm_function_references(true);
        config.wasm_tail_call(true);
        config.wasm_exceptions(true);
        config.collector(wasmtime::Collector::Copying);
        config.wasm_threads(matches!(backend, ExecutionBackend::Native));
        #[cfg(target_os = "macos")]
        let mut jit = None;
        match backend {
            ExecutionBackend::Native => {
                if cfg!(target_os = "ios") {
                    return Err(RuntimeError::Engine(
                        "iOS hosts require the Pulley interpreter".to_owned(),
                    ));
                }
                #[cfg(target_os = "macos")]
                {
                    let memory = std::sync::Arc::new(jit_memory::JitCodeMemory::new());
                    config.with_custom_code_memory(Some(memory.clone()));
                    jit = Some(memory);
                }
            }
            ExecutionBackend::Pulley => {
                let target = match (usize::BITS, cfg!(target_endian = "little")) {
                    (32, true) => "pulley32",
                    (32, false) => "pulley32be",
                    (64, true) => "pulley64",
                    (64, false) => "pulley64be",
                    _ => {
                        return Err(RuntimeError::Engine(
                            "unsupported Pulley host pointer width".to_owned(),
                        ));
                    }
                };
                config.target(target).map_err(|error| RuntimeError::Engine(error.to_string()))?;
                config.signals_based_traps(false);
                config.memory_reservation(DEFAULT_MEMORY_LIMIT as u64);
                config.memory_guard_size(0);
                config.guard_before_linear_memory(false);
            }
        }
        let engine = with_signals_blocked(|| Engine::new(&config))
            .map_err(|error| RuntimeError::Engine(error.to_string()))?;
        Ok(Self {
            engine,
            fuel_limit,
            cache: None,
            compile_gate: Default::default(),
            #[cfg(target_os = "macos")]
            jit,
            #[cfg(test)]
            compilations: Default::default(),
        })
    }

    /// Loading accepts only an owned image produced by our compiler or verified
    /// against an authenticated engine descriptor. Never map a mutable cache file.
    #[allow(unsafe_code, reason = "Only trusted compiler images enter owned code memory.")]
    pub(crate) fn load_image(&self, image: &compiled::Image) -> wasmtime::Result<Module> {
        let _timing = crate::trace::Span::new("runtime.module_load");
        if image.descriptor.compatibility != self.compatibility() {
            return Err(wasmtime::Error::msg("compiled image configuration mismatch"));
        }
        #[cfg(target_os = "macos")]
        if let Some(jit) = &self.jit {
            let memory = jit
                .place(&image.bytes)
                .map_err(|error| wasmtime::Error::new(error).context("mapping guest code"))?;
            // SAFETY: trusted compiler output is copied into owned MAP_JIT
            // memory, which remains immutable until Wasmtime unpublishes it.
            return unsafe { Module::deserialize_raw(&self.engine, memory) };
        }
        // SAFETY: bytes are compiler output, owned and immutable during loading.
        unsafe { Module::deserialize(&self.engine, &image.bytes) }
    }

    #[cfg(test)]
    pub(crate) fn module(&self, wasm: &[u8]) -> wasmtime::Result<Module> {
        let bytes = self.engine.precompile_module(wasm)?;
        let image = compiled::Image {
            descriptor: compiled::Descriptor {
                wasm: *blake3::hash(wasm).as_bytes(),
                compatibility: self.compatibility(),
                image: *blake3::hash(&bytes).as_bytes(),
                bytes: bytes.len() as u64,
            },
            bytes,
        };
        self.load_image(&image)
    }

    /// Parse, check and compile a guest module. Compilation is the expensive
    /// step of loading a guest, so a guest is compiled once per load and the
    /// result shared between validation and instantiation.
    fn compile_app(&self, wasm: &[u8]) -> Result<Module, RuntimeError> {
        let _timing = crate::trace::Span::new("runtime.compile_app");
        let wasm_digest = *blake3::hash(wasm).as_bytes();
        let compatibility = self.compatibility();
        if let Some(image) =
            self.cache.as_ref().and_then(|cache| cache.load(wasm_digest, compatibility))
        {
            match self.load_image(&image) {
                Ok(module) => return Ok(module),
                Err(error) => eprintln!("Compiled module cache load failed; recompiling: {error}"),
            }
        }
        // The existing local-import and AP workers are the bounded callers.
        // Coalesce simultaneous cold preparations without holding the cache's
        // descriptor lock or blocking the engine's interactive owner.
        let _compile_guard = self
            .compile_gate
            .lock()
            .map_err(|_| RuntimeError::Engine("compiler admission lock poisoned".into()))?;
        if let Some(image) =
            self.cache.as_ref().and_then(|cache| cache.load(wasm_digest, compatibility))
            && let Ok(module) = self.load_image(&image)
        {
            return Ok(module);
        }
        #[cfg(test)]
        self.compilations.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        for payload in wasmtime::wasmparser::Parser::new(0).parse_all(wasm) {
            let payload =
                payload.map_err(|error| RuntimeError::InvalidModule(error.to_string()))?;
            if matches!(payload, wasmtime::wasmparser::Payload::StartSection { .. }) {
                return Err(RuntimeError::UnsupportedStartFunction);
            }
        }
        let prepared = {
            let _timing = crate::trace::Span::new("runtime.prepare_atomics");
            wasm_atomics::prepare(wasm, self.engine.get_wasm_features(), self.engine.is_pulley())
                .map_err(RuntimeError::InvalidModule)?
        };
        let bytes = {
            let _compile = crate::trace::Span::new("runtime.module_compile");
            self.engine
                .precompile_module(prepared.as_ref())
                .map_err(|error| RuntimeError::InvalidModule(error.to_string()))?
        };
        let image = compiled::Image {
            descriptor: compiled::Descriptor {
                wasm: wasm_digest,
                compatibility,
                image: *blake3::hash(&bytes).as_bytes(),
                bytes: bytes.len() as u64,
            },
            bytes,
        };
        let module = self
            .load_image(&image)
            .map_err(|error| RuntimeError::InvalidModule(error.to_string()))?;
        if let Some(cache) = &self.cache {
            cache.publish(&image);
        }
        Ok(module)
    }
}

#[cfg(test)]
mod tests;
