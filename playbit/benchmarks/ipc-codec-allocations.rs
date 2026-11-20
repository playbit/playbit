//! Counts requested heap bytes on the benchmark thread only, in separate untimed passes.
#![allow(
    unsafe_code,
    reason = "The diagnostic allocator forwards the GlobalAlloc contract to System."
)]
use std::{
    alloc::{GlobalAlloc, Layout, System},
    cell::Cell,
};

#[derive(Clone, Copy, Default, serde::Serialize)]
pub struct Counts {
    allocations: usize,
    reallocations: usize,
    requested_bytes: usize,
}
thread_local! {
    static COUNTS: Cell<Option<Counts>> = const { Cell::new(None) };
}
struct Allocator;
#[global_allocator]
static ALLOCATOR: Allocator = Allocator;

fn record(bytes: usize, reallocation: bool) {
    let _ = COUNTS.try_with(|state| {
        if let Some(mut counts) = state.get() {
            counts.allocations += usize::from(!reallocation);
            counts.reallocations += usize::from(reallocation);
            counts.requested_bytes += bytes;
            state.set(Some(counts));
        }
    });
}
unsafe impl GlobalAlloc for Allocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc(layout) };
        if !pointer.is_null() {
            record(layout.size(), false);
        }
        pointer
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc_zeroed(layout) };
        if !pointer.is_null() {
            record(layout.size(), false);
        }
        pointer
    }
    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        let pointer = unsafe { System.realloc(pointer, layout, size) };
        if !pointer.is_null() {
            record(size, true);
        }
        pointer
    }
    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        unsafe { System.dealloc(pointer, layout) };
    }
}

pub fn measure<T>(action: impl FnOnce() -> T) -> (T, Counts) {
    COUNTS.with(|state| state.set(Some(Counts::default())));
    let result = action();
    let counts = COUNTS.with(|state| state.replace(None).unwrap_or_default());
    (result, counts)
}
