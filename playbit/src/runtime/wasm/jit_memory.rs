//! Executable memory for natively compiled guests on macOS.
//!
//! The macOS app is signed with the hardened runtime and only the
//! `com.apple.security.cs.allow-jit` entitlement, so pages may become
//! executable only inside a mapping created with `MAP_JIT`. Wasmtime maps
//! compiled code with a plain anonymous `mmap` and the hardened runtime then
//! kills the process at the first `mprotect(PROT_EXEC)`. The alternative
//! entitlement, `allow-unsigned-executable-memory`, would let any memory-safety
//! bug in the host turn arbitrary pages executable, so it is not used.
//!
//! Rather than patching Wasmtime, [`Runtime`](super::Runtime) compiles a guest
//! to an image with `Engine::precompile_module`, copies the image into a
//! `MAP_JIT` mapping owned here, and loads it with `Module::deserialize_raw`.
//! Wasmtime's `CustomCodeMemory` hook then asks this type to publish the text
//! section, which is an ordinary `mprotect` to read/execute on that mapping.
//! Nothing is written to a mapping after publication, so the per-thread JIT
//! write-protection APIs are not needed.
//!
//! Wasmtime unpublishes a mapping only when the module's code memory is
//! dropped (the `debug` feature, which can unpublish a live module, is not
//! enabled). Pages are unmapped lazily, at the next placement or when the
//! engine drops this object, so the unmap never races with Wasmtime's own
//! teardown of the module.

#![allow(
    unsafe_code,
    reason = "MAP_JIT ownership implements Wasmtime unsafe memory interfaces; each operation documents its invariant."
)]
use std::ptr::NonNull;
use std::sync::Mutex;

use wasmtime::CustomCodeMemory;

unsafe extern "C" {
    fn sys_icache_invalidate(start: *mut libc::c_void, len: usize);
}

struct Mapping {
    base: *mut u8,
    len: usize,
    released: bool,
}

// The raw pointer is only dereferenced through libc calls under the mutex.
unsafe impl Send for Mapping {}

impl Mapping {
    fn contains(&self, ptr: *const u8, len: usize) -> bool {
        let start = ptr as usize;
        let base = self.base as usize;
        start >= base && start.saturating_add(len) <= base + self.len
    }

    fn unmap(&self) {
        // SAFETY: `base`/`len` came from our own successful `mmap`.
        let rc = unsafe { libc::munmap(self.base.cast(), self.len) };
        debug_assert_eq!(rc, 0, "munmap: {}", std::io::Error::last_os_error());
    }
}

pub(super) struct JitCodeMemory {
    mappings: Mutex<Vec<Mapping>>,
}

impl JitCodeMemory {
    pub(super) fn new() -> Self {
        Self { mappings: Mutex::new(Vec::new()) }
    }

    /// Copies a compiled image into a fresh `MAP_JIT` mapping. The returned
    /// memory stays valid until Wasmtime unpublishes it; only the next
    /// placement or dropping this object unmaps released pages.
    pub(super) fn place(&self, image: &[u8]) -> std::io::Result<NonNull<[u8]>> {
        let page = page_size();
        let len = image.len().max(1).div_ceil(page) * page;
        let mut mappings = self.mappings.lock().unwrap_or_else(|e| e.into_inner());
        mappings.retain(|mapping| {
            if mapping.released {
                mapping.unmap();
            }
            !mapping.released
        });
        // SAFETY: anonymous mapping with no file; arguments are well formed.
        let base = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANON | libc::MAP_JIT,
                -1,
                0,
            )
        };
        if base == libc::MAP_FAILED {
            return Err(std::io::Error::last_os_error());
        }
        let base = base.cast::<u8>();
        // SAFETY: `base` is a fresh writable mapping of at least `image.len()`
        // bytes that nothing else references yet.
        unsafe { std::ptr::copy_nonoverlapping(image.as_ptr(), base, image.len()) };
        mappings.push(Mapping { base, len, released: false });
        Ok(NonNull::slice_from_raw_parts(
            NonNull::new(base).expect("mmap returned a null mapping"),
            image.len(),
        ))
    }

    fn protect(
        &self,
        ptr: *const u8,
        len: usize,
        prot: libc::c_int,
        release: bool,
    ) -> wasmtime::Result<()> {
        let mut mappings = self.mappings.lock().unwrap_or_else(|e| e.into_inner());
        let mapping = mappings
            .iter_mut()
            .find(|mapping| !mapping.released && mapping.contains(ptr, len))
            .ok_or_else(|| {
                wasmtime::Error::msg("guest code was not placed by the runtime's JIT code memory")
            })?;
        // SAFETY: the range lies inside a mapping we own; Wasmtime guarantees
        // page alignment via `required_alignment`.
        if unsafe { libc::mprotect(ptr.cast_mut().cast(), len, prot) } != 0 {
            return Err(wasmtime::Error::new(std::io::Error::last_os_error())
                .context("changing guest code protection"));
        }
        if prot & libc::PROT_EXEC != 0 {
            // SAFETY: the range was just written and is about to be executed.
            unsafe { sys_icache_invalidate(ptr.cast_mut().cast(), len) };
        }
        mapping.released = release;
        Ok(())
    }
}

impl Drop for JitCodeMemory {
    fn drop(&mut self) {
        // Every module's code memory holds a reference to this object, so no
        // mapping can still be in use here.
        for mapping in self.mappings.get_mut().unwrap_or_else(|e| e.into_inner()).drain(..) {
            mapping.unmap();
        }
    }
}

impl CustomCodeMemory for JitCodeMemory {
    fn required_alignment(&self) -> usize {
        page_size()
    }

    fn publish_executable(&self, ptr: *const u8, len: usize) -> wasmtime::Result<()> {
        self.protect(ptr, len, libc::PROT_READ | libc::PROT_EXEC, false)
    }

    fn unpublish_executable(&self, ptr: *const u8, len: usize) -> wasmtime::Result<()> {
        self.protect(ptr, len, libc::PROT_READ | libc::PROT_WRITE, true)
    }
}

fn page_size() -> usize {
    // SAFETY: plain sysconf query.
    let size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    usize::try_from(size).expect("host page size")
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert successful fixture operations.")]
mod tests {
    use super::*;

    #[test]
    fn placed_images_are_page_aligned_and_unmapped_after_release() {
        let memory = JitCodeMemory::new();
        let image = vec![0xAAu8; 3 * page_size() + 17];
        let placed = memory.place(&image);
        let placed = placed.expect("MAP_JIT mapping");
        assert_eq!(placed.len(), image.len());
        assert_eq!(placed.as_ptr().cast::<u8>() as usize % page_size(), 0);
        let base = placed.as_ptr().cast::<u8>().cast_const();
        // SAFETY: freshly placed, fully initialized copy of `image`.
        assert_eq!(unsafe { *base.add(image.len() - 1) }, 0xAA);
        memory.publish_executable(base, page_size()).expect("publish inside mapping");
        memory.unpublish_executable(base, page_size()).expect("unpublish inside mapping");
        assert_eq!(memory.mappings.lock().unwrap().len(), 1);
        memory.place(&image[..1]).expect("second placement");
        assert_eq!(memory.mappings.lock().unwrap().len(), 1, "released mapping was unmapped");
    }

    #[test]
    fn foreign_memory_is_refused() {
        let memory = JitCodeMemory::new();
        let foreign = vec![0u8; page_size()];
        let error = memory.publish_executable(foreign.as_ptr(), page_size()).unwrap_err();
        assert!(error.to_string().contains("not placed"), "{error}");
    }
}
