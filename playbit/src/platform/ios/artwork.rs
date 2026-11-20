//! A display-only read independent of the Engine owner. UIKit calls on its single
//! artwork worker and supplies one fixed-size pixel buffer, never an IPC payload.
use crate::identifier::valid as valid_id;
use crate::package::Package;
use std::{
    ffi::{CStr, c_char},
    fs::File,
    path::Path,
};

const PIXELS: usize = 120 * 120 * 4;

/// `profile` and `build` must be valid NUL-terminated UTF-8 strings; `output`
/// must point to `capacity` writable bytes and `present` to writable u8 storage.
/// Success returns null, with present=0 for missing artwork or present=1 after
/// writing pixels. Failure sets present=0 and returns an owned UTF-8 error freed
/// with pb_ios_string_free. Does not retain any caller memory.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pb_ios_icon_read(
    profile: *const c_char,
    build: *const c_char,
    published: u8,
    output: *mut u8,
    capacity: usize,
    present: *mut u8,
) -> *mut c_char {
    super::native_result(|| {
        let present = unsafe { present.as_mut() }.ok_or("missing artwork result")?;
        *present = 0;
        if profile.is_null()
            || build.is_null()
            || output.is_null()
            || capacity != PIXELS
            || published > 1
        {
            return Err("invalid artwork buffer".into());
        }
        let profile = unsafe { CStr::from_ptr(profile) }.to_str().map_err(|e| e.to_string())?;
        let build = unsafe { CStr::from_ptr(build) }.to_str().map_err(|e| e.to_string())?;
        if !valid_id(build, 32) {
            return Err("invalid build identity".into());
        }
        let bytes = {
            let _timing = crate::trace::Span::new("ios.artwork.read");
            if published == 1 {
                let display = crate::engine::app_display::read(Path::new(profile), build)
                    .map_err(|e| e.to_string())?;
                if let Some(error) = display.icon_error {
                    return Err(error);
                }
                display.icon.map(|hex| {
                    hex.as_bytes()
                        .as_chunks::<2>()
                        .0
                        .iter()
                        .map(|pair| {
                            let digit = |b: u8| if b <= b'9' { b - b'0' } else { b - b'a' + 10 };
                            digit(pair[0]) * 16 + digit(pair[1])
                        })
                        .collect::<Vec<_>>()
                })
            } else {
                let mut file =
                    File::open(Path::new(profile).join(crate::paths::BUILDS).join(build))
                        .map_err(|e| e.to_string())?;
                Package::read_display_icon(&mut file).map_err(|e| e.to_string())?
            }
        };
        let Some(bytes) = bytes else {
            return Ok(());
        };
        let _timing = crate::trace::Span::new("ios.artwork.decode");
        let mut pixels = crate::package::artwork::decode(&bytes, 120)?.into_raw();
        // CoreGraphics consumes premultiplied RGBA. Convert on this worker so
        // UIKit does not need to prepare straight-alpha pixels during display.
        for pixel in pixels.as_chunks_mut::<4>().0 {
            let alpha = pixel[3] as u16;
            for channel in &mut pixel[..3] {
                *channel = ((*channel as u16 * alpha + 127) / 255) as u8;
            }
        }
        unsafe {
            std::ptr::copy_nonoverlapping(pixels.as_ptr(), output, PIXELS);
        }
        *present = 1;
        Ok(())
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert successful fixture operations.")]
mod tests {
    use super::*;
    use std::{collections::BTreeMap, ffi::CString};
    fn read(profile: &Path, id: &str, pixels: &mut [u8]) -> Result<bool, String> {
        read_source(profile, id, 0, pixels)
    }
    fn read_source(
        profile: &Path,
        id: &str,
        published: u8,
        pixels: &mut [u8],
    ) -> Result<bool, String> {
        let profile = CString::new(profile.to_str().unwrap()).unwrap();
        let id = CString::new(id).unwrap();
        unsafe {
            let mut present = 99;
            let response = pb_ios_icon_read(
                profile.as_ptr(),
                id.as_ptr(),
                published,
                pixels.as_mut_ptr(),
                pixels.len(),
                &mut present,
            );
            if response.is_null() {
                assert!(present <= 1);
                Ok(present == 1)
            } else {
                assert_eq!(present, 0);
                let error = CStr::from_ptr(response).to_str().unwrap().to_owned();
                crate::platform::ios::pb_ios_string_free(response);
                Err(error)
            }
        }
    }
    #[test]
    fn published_cache_uses_shared_decoder_without_a_local_executable() {
        let dir = tempfile::tempdir().unwrap();
        let build = crate::identifier::encode(&[0xab; 32]);
        let folder = dir.path().join(crate::paths::APP_DISPLAYS);
        std::fs::create_dir_all(&folder).unwrap();
        let icon = br#"<svg width="24" height="24"><rect width="24" height="24" fill="red" opacity="0.5"/></svg>"#;
        let mut display = crate::network::ap::operation::BuildDisplay {
            app_id: "test".into(),
            build_id: build.clone(),
            name: "TODO".into(),
            icon: Some(icon.iter().map(|b| format!("{b:02x}")).collect()),
            icon_error: None,
        };
        let path = folder.join(&build);
        std::fs::write(&path, serde_json::to_vec(&display).unwrap()).unwrap();
        let mut pixels = vec![99; PIXELS];
        assert!(read_source(dir.path(), &build, 1, &mut pixels).unwrap());
        assert!(pixels.as_chunks::<4>().0.iter().all(|p| *p == [128, 0, 0, 128]));
        assert!(!dir.path().join(crate::paths::BUILDS).exists());
        pixels.fill(99);
        assert!(read_source(dir.path(), &build, 2, &mut pixels).is_err());
        display.icon = None;
        display.icon_error = Some("Artwork too large".into());
        std::fs::write(&path, serde_json::to_vec(&display).unwrap()).unwrap();
        assert!(read_source(dir.path(), &build, 1, &mut pixels).is_err());
        assert!(pixels.iter().all(|p| *p == 99));
        display.icon_error = None;
        std::fs::write(&path, serde_json::to_vec(&display).unwrap()).unwrap();
        assert!(!read_source(dir.path(), &build, 1, &mut pixels).unwrap());
    }
    #[test]
    fn native_buffer_renders_and_rejects_bad_inputs_without_touching_output() {
        let profile = tempfile::tempdir().unwrap();
        std::fs::create_dir(profile.path().join(crate::paths::BUILDS)).unwrap();
        let icon = br#"<svg width="24" height="24"><rect width="24" height="24" fill="red" opacity="0.5"/></svg>"#.to_vec();
        let mut package = Package::new(
            "test".into(),
            "Artwork".into(),
            b"\0asm\x01\0\0\0".to_vec(),
            BTreeMap::from([(crate::package::codec::ICON.into(), icon)]),
        )
        .unwrap();

        let id = package.id().unwrap();
        std::fs::write(
            profile.path().join(crate::paths::BUILDS).join(&id),
            package.to_bytes().unwrap(),
        )
        .unwrap();
        let mut pixels = vec![99; PIXELS];
        let result = read(profile.path(), &id, &mut pixels);
        assert!(result.unwrap());
        assert!(pixels.as_chunks::<4>().0.iter().all(|p| *p == [128, 0, 0, 128]));
        pixels.fill(99);
        for id in ["../outside".to_owned(), crate::identifier::encode(&[0xff; 32])] {
            assert!(read(profile.path(), &id, &mut pixels).is_err());
            assert!(pixels.iter().all(|p| *p == 99));
        }
        assert!(read(profile.path(), &id, &mut pixels[..PIXELS - 1]).is_err());
        assert!(pixels.iter().all(|p| *p == 99));
        package = Package::new(
            "test".into(),
            package.name().to_owned(),
            package.wasm().unwrap(),
            Default::default(),
        )
        .unwrap();
        let id = package.id().unwrap();
        std::fs::write(
            profile.path().join(crate::paths::BUILDS).join(&id),
            package.to_bytes().unwrap(),
        )
        .unwrap();
        assert!(!read(profile.path(), &id, &mut pixels).unwrap());
        assert!(pixels.iter().all(|p| *p == 99));
    }
}
