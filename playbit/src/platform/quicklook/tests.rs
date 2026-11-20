//! Exercise the private ABI with real canonical bundles and borrowed descriptors.
#![allow(clippy::unwrap_used, reason = "Tests assert fixture construction and ABI results.")]
use super::*;
use crate::package::codec::{self, Source};
use std::{collections::BTreeMap, io::Cursor, os::fd::AsRawFd};

fn package(icon: Option<Vec<u8>>) -> Vec<u8> {
    let files = icon.map(|icon| (codec::ICON.into(), icon)).into_iter().collect();
    Package::new("test".into(), "Thumbnail".into(), b"\0asm\x01\0\0\0".to_vec(), files)
        .unwrap()
        .to_bytes()
        .unwrap()
}

fn render(bytes: &[u8], side: u32, length: usize) -> (i32, Vec<u8>) {
    let file = tempfile::tempfile().unwrap();
    std::os::unix::fs::FileExt::write_all_at(&file, bytes, 0).unwrap();
    let mut pixels = vec![7; length];
    // SAFETY: a live descriptor and exclusive output remain valid for the call.
    let result = unsafe {
        pb_thumbnail_render(file.as_raw_fd(), side, 0, pixels.as_mut_ptr(), pixels.len())
    };
    assert!(file.metadata().is_ok(), "the bridge must not close a borrowed descriptor");
    (result, pixels)
}

#[test]
fn raster_and_vector_artwork_preserve_alpha_and_orientation() {
    // Use asymmetric colors and transparency to detect channel swaps and flips.
    let mut raster = image::RgbaImage::from_pixel(2, 2, image::Rgba([255, 0, 0, 128]));
    raster.put_pixel(0, 1, image::Rgba([0, 0, 255, 255]));
    let mut png = Cursor::new(Vec::new());
    raster.write_to(&mut png, image::ImageFormat::Png).unwrap();
    let mut jpeg = Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(raster)
        .to_rgb8()
        .write_to(&mut jpeg, image::ImageFormat::Jpeg)
        .unwrap();
    let mut large_png = Cursor::new(Vec::new());
    image::RgbaImage::from_pixel(1024, 1024, image::Rgba([255, 0, 0, 128]))
        .write_to(&mut large_png, image::ImageFormat::Png)
        .unwrap();
    let svg = br##"<svg xmlns="http://www.w3.org/2000/svg" width="2" height="2"><path fill="#f00" fill-opacity=".5" d="M0 0h2v2H0z"/></svg>"##.to_vec();
    for icon in [png.into_inner(), jpeg.into_inner(), large_png.into_inner(), svg] {
        for side in [128, 256, 512] {
            let expected = artwork::macos_icon(&icon, side, false).unwrap().into_raw();
            let (status, pixels) = render(&package(Some(icon.clone())), side, expected.len());
            assert_eq!(status, 0);
            assert_eq!(pixels, expected);
            let center = (side as usize * side as usize / 2 + side as usize / 2) * 4;
            assert!(pixels[center] > pixels[center + 2]);
        }
    }
}

#[test]
fn invalid_inputs_and_absent_artwork_return_explicit_status() {
    // Errors cannot write a partial image or claim that the descriptor was consumed.
    let valid = package(Some(b"not an image".to_vec()));
    for (bytes, side, length, expected) in [
        (valid.clone(), 128, 128 * 128 * 4, -1),
        (valid.clone(), 127, 128 * 128 * 4, -1),
        (valid.clone(), 512, 1, -1),
        (valid[..40].to_vec(), 128, 128 * 128 * 4, -1),
        (package(None), 128, 128 * 128 * 4, 1),
    ] {
        let (result, pixels) = render(&bytes, side, length);
        assert_eq!(result, expected);
        assert!(pixels.iter().all(|byte| *byte == 7));
    }
    let mut version = package(None);
    version[8] = 2;
    assert_eq!(render(&version, 128, 128 * 128 * 4).0, -1);
    // SAFETY: rejected arguments do not dereference the null pointer or descriptor.
    assert_eq!(unsafe { pb_thumbnail_render(-1, 128, 0, std::ptr::null_mut(), 0) }, -1);
    let directory = tempfile::tempdir().unwrap();
    let file = File::open(directory.path()).unwrap();
    let mut pixels = vec![0; 128 * 128 * 4];
    // SAFETY: valid descriptor and exclusive output; directories must be rejected.
    assert_eq!(
        unsafe { pb_thumbnail_render(file.as_raw_fd(), 128, 0, pixels.as_mut_ptr(), pixels.len()) },
        -1
    );
}

#[test]
fn display_reads_skip_large_assets_and_enforce_icon_limits() {
    // Count actual reader bytes, then exercise the same indexed reader through FFI.
    use std::io::{Read, Seek, SeekFrom};
    struct Counted {
        file: File,
        read: usize,
    }
    impl Read for Counted {
        fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
            let count = self.file.read(bytes)?;
            self.read += count;
            Ok(count)
        }
    }
    impl Seek for Counted {
        fn seek(&mut self, position: SeekFrom) -> std::io::Result<u64> {
            self.file.seek(position)
        }
    }
    let icon = br##"<svg xmlns="http://www.w3.org/2000/svg" width="1" height="1"><path fill="#f00" d="M0 0h1v1H0z"/></svg>"##;
    for size in [60, 127] {
        let asset = tempfile::tempfile().unwrap();
        asset.set_len(size * 1024 * 1024).unwrap();
        let mut sources = BTreeMap::from([
            (
                codec::MANIFEST.into(),
                Source::Bytes(br#"{"app_id":"test","name":"Thumbnail"}"#.to_vec()),
            ),
            (codec::EXECUTABLE.into(), Source::Bytes(b"\0asm\x01\0\0\0".to_vec())),
            (
                codec::INITIALIZERS.into(),
                Source::Bytes(
                    br#"{"root":{"permissions":{"grants":[]},"databases":[],"blobs":[]}}"#.to_vec(),
                ),
            ),
            (codec::ICON.into(), Source::Bytes(icon.to_vec())),
            ("large-asset".into(), Source::File(asset)),
        ]);
        let mut file = tempfile::tempfile().unwrap();
        codec::write(&sources, &mut file).unwrap();
        let mut input = Counted { file, read: 0 };
        assert_eq!(Package::read_display_icon(&mut input).unwrap().unwrap(), icon);
        assert!(input.read < 1024, "display reads must be independent of unused payload size");
        let mut pixels = vec![0; 128 * 128 * 4];
        // SAFETY: live file and exclusive output are retained through rendering.
        assert_eq!(
            unsafe {
                pb_thumbnail_render(
                    input.file.as_raw_fd(),
                    128,
                    0,
                    pixels.as_mut_ptr(),
                    pixels.len(),
                )
            },
            0
        );
        sources.insert(codec::ICON.into(), Source::Bytes(vec![0; codec::MAX_ICON_BYTES + 1]));
        assert!(codec::write(&sources, &mut Vec::new()).is_err());
    }
}

#[test]
fn bridge_uses_the_shared_dark_template_and_rejects_unknown_appearance() {
    let svg = br#"<svg width="1" height="1"><rect width="1" height="1" fill="red"/></svg>"#;
    let file = tempfile::tempfile().unwrap();
    std::os::unix::fs::FileExt::write_all_at(&file, &package(Some(svg.to_vec())), 0).unwrap();
    for dark in [0, 1, 2] {
        let mut pixels = vec![7; 128 * 128 * 4];
        // SAFETY: a live regular file and exclusive output are retained through the call.
        let result = unsafe {
            pb_thumbnail_render(file.as_raw_fd(), 128, dark, pixels.as_mut_ptr(), pixels.len())
        };
        if dark > 1 {
            assert_eq!(result, -1);
            assert!(pixels.iter().all(|byte| *byte == 7));
        } else {
            assert_eq!(result, 0);
            assert_eq!(pixels, artwork::macos_icon(svg, 128, dark != 0).unwrap().into_raw());
        }
    }
}
