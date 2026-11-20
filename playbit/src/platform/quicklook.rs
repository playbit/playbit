//! Display-only Finder bridge. The caller lends a regular-file descriptor and
//! owns the RGBA buffer. No executable admission, profile or network is involved.
#![allow(unsafe_code, reason = "The private Swift bridge lends a descriptor and output slice.")]
use crate::package::{Package, artwork};
use std::{fs::File, mem::ManuallyDrop, os::fd::FromRawFd};

/// Render straight RGBA into a caller-owned, exactly sized square buffer.
/// Returns 0 for success, 1 for absent artwork, and -1 for invalid input or I/O.
/// The descriptor's seek position may change; the descriptor is never closed.
///
/// # Safety
/// `descriptor` must remain a valid, exclusively borrowed descriptor throughout
/// this call. `pixels` must point to `length` writable bytes with no aliases.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pb_thumbnail_render(
    descriptor: i32,
    side: u32,
    dark: u32,
    pixels: *mut u8,
    length: usize,
) -> i32 {
    if descriptor < 0 || !matches!(side, 128 | 256 | 512) || dark > 1 || pixels.is_null() {
        return -1;
    }
    if length != side as usize * side as usize * 4 {
        return -1;
    }
    // SAFETY: the bridge lends a live descriptor. ManuallyDrop preserves the
    // caller's ownership on every return, including errors and unwinding.
    let mut file = ManuallyDrop::new(unsafe { File::from_raw_fd(descriptor) });
    let Ok(metadata) = file.metadata() else { return -1 };
    if !metadata.is_file() {
        return -1;
    }
    let bytes = match Package::read_display_icon(&mut *file) {
        Ok(Some(bytes)) => bytes,
        Ok(None) => return 1,
        Err(_) => return -1,
    };
    let Ok(image) = artwork::macos_icon(&bytes, side, dark != 0) else { return -1 };
    // SAFETY: the caller supplies this exclusive, exactly sized output region.
    let output = unsafe { std::slice::from_raw_parts_mut(pixels, length) };
    output.copy_from_slice(image.as_raw());
    0
}

#[cfg(test)]
#[path = "quicklook/tests.rs"]
mod tests;
