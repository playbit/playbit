//! Keep large transient MCP buffers out of Darwin's deferred malloc cache.
//! No other executable uses this allocation policy.
#![allow(
    unsafe_code,
    reason = "The allocator and native resource probes implement their required unsafe interfaces."
)]
use std::alloc::{GlobalAlloc, Layout, System};

struct Allocator;
#[global_allocator]
static ALLOCATOR: Allocator = Allocator;

fn mapped(layout: Layout) -> bool {
    // Supported macOS pages are at least 4 KiB. Larger alignments keep the
    // system implementation; decoded JSON buffers need at most 8 bytes.
    layout.size() >= 64 * 1024 && layout.align() <= 4096
}

// SAFETY: mmap/munmap and System are thread-safe. The same layout predicate is
// used for allocation and release; there is no shared mutable bookkeeping.
unsafe impl GlobalAlloc for Allocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if !mapped(layout) {
            return unsafe { System.alloc(layout) };
        }
        // SAFETY: a nonzero size requests a private writable mapping, without a
        // file or fixed address. Kernel page alignment satisfies the predicate.
        let pointer = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                layout.size(),
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANON,
                -1,
                0,
            )
        };
        if pointer == libc::MAP_FAILED { std::ptr::null_mut() } else { pointer.cast() }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // Anonymous mappings are zero-filled; System supplies the same contract.
        if mapped(layout) {
            unsafe { self.alloc(layout) }
        } else {
            unsafe { System.alloc_zeroed(layout) }
        }
    }
    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        if mapped(layout) {
            // SAFETY: GlobalAlloc requires the original pointer and layout.
            // munmap rounds the same non-page-multiple length as mmap.
            unsafe {
                libc::munmap(pointer.cast(), layout.size());
            }
        } else {
            unsafe {
                System.dealloc(pointer, layout);
            }
        }
    }
    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        // SAFETY: GlobalAlloc requires a nonzero valid new size and retains alignment.
        let next = unsafe { Layout::from_size_align_unchecked(size, layout.align()) };
        if !mapped(layout) && !mapped(next) {
            return unsafe { System.realloc(pointer, layout, size) };
        }
        let replacement = unsafe { self.alloc(next) };
        if !replacement.is_null() {
            // SAFETY: distinct live allocations cover at least the smaller size.
            unsafe {
                std::ptr::copy_nonoverlapping(pointer, replacement, layout.size().min(size));
            }
            unsafe {
                self.dealloc(pointer, layout);
            }
        }
        // A failed allocation leaves the original live, as realloc requires.
        replacement
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert successful fixture operations.")]
mod tests {
    use super::*;

    #[test]
    fn zeroed_aligned_buffers_preserve_bytes_across_both_mapping_boundaries() {
        for alignment in [1, 8, 4096, 32768] {
            let mut layout = Layout::from_size_align(65535, alignment).unwrap();
            let mut pointer = unsafe { ALLOCATOR.alloc_zeroed(layout) };
            assert!(!pointer.is_null());
            assert!(
                unsafe { std::slice::from_raw_parts(pointer, layout.size()) }
                    .iter()
                    .all(|byte| *byte == 0)
            );
            for size in [65536, 65537, 256 * 1024 + 3, 32768] {
                assert_eq!(pointer as usize % alignment, 0);
                unsafe {
                    std::ptr::write_bytes(pointer, 0xa5, layout.size());
                }
                let replacement = unsafe { ALLOCATOR.realloc(pointer, layout, size) };
                assert!(!replacement.is_null());
                assert_eq!(replacement as usize % alignment, 0);
                assert!(
                    unsafe { std::slice::from_raw_parts(replacement, layout.size().min(size)) }
                        .iter()
                        .all(|byte| *byte == 0xa5)
                );
                pointer = replacement;
                layout = Layout::from_size_align(size, alignment).unwrap();
            }
            unsafe {
                ALLOCATOR.dealloc(pointer, layout);
            }
        }
        let layout = Layout::from_size_align(128 * 1024 + 1, 8).unwrap();
        let pointer = unsafe { ALLOCATOR.alloc_zeroed(layout) };
        assert!(!pointer.is_null());
        assert!(
            unsafe { std::slice::from_raw_parts(pointer, layout.size()) }
                .iter()
                .all(|byte| *byte == 0)
        );
        unsafe {
            ALLOCATOR.dealloc(pointer, layout);
        }
    }

    #[test]
    fn failed_mapping_resize_keeps_the_original_allocation() {
        let layout = Layout::from_size_align(64 * 1024, 8).unwrap();
        let pointer = unsafe { ALLOCATOR.alloc(layout) };
        assert!(!pointer.is_null());
        unsafe {
            std::ptr::write_bytes(pointer, 0x3c, layout.size());
        }
        // This exceeds macOS's user virtual-address space; mmap must fail
        // without reserving physical memory or invoking Rust's OOM handler.
        let impossible = (isize::MAX as usize) & !4095;
        assert!(unsafe { ALLOCATOR.realloc(pointer, layout, impossible) }.is_null());
        assert!(
            unsafe { std::slice::from_raw_parts(pointer, layout.size()) }
                .iter()
                .all(|byte| *byte == 0x3c)
        );
        unsafe {
            ALLOCATOR.dealloc(pointer, layout);
        }
    }
}
