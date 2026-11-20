use super::*;
#[derive(Clone, Copy)]
#[repr(C)]
pub struct Volume {
    pub size: u32,
    pub written: u32,
    pub id: [u8; 16],
    pub owner: [u8; 16],
    pub name: [u8; 256],
    pub name_length: u32,
    pub share_key_with_service: u32,
}
memory::record!(Volume, 304, {
    id: [u8; 16], owner: [u8; 16], name: [u8; 256],
    name_length: u32, share_key_with_service: u32,
});
fn record(volume: crate::store::Volume) -> Result<Volume> {
    if volume.name.len() > 256 {
        return Err(Error::LimitExceeded);
    }
    let mut name = [0; 256];
    name[..volume.name.len()].copy_from_slice(volume.name.as_bytes());
    Ok(Volume {
        size: std::mem::size_of::<Volume>() as u32,
        written: 0,
        id: *volume.id.as_bytes(),
        owner: *volume.owner.as_bytes(),
        name,
        name_length: volume.name.len() as u32,
        share_key_with_service: u32::from(volume.share_key_with_service),
    })
}
/// # Safety
/// The handle and disjoint stable input/writable output obey playbit/engine.h.
#[cfg_attr(not(all(target_os = "macos", feature = "macos-shared-runtime")), unsafe(no_mangle))]
pub unsafe extern "C" fn pb_engine_volume_create(
    engine: *mut Engine,
    label: *const u8,
    length: u32,
    output: *mut Volume,
) -> i32 {
    // SAFETY: admitted output precedes the commit; input is borrowed for this call.
    unsafe {
        with_engine(engine, |inner| {
            let destination = memory::native_region(output, true)?;
            let source = input(label, length, 256)?;
            disjoint(&destination, &source)?;
            memory::clear_native(output)?;
            let label = std::str::from_utf8(std::slice::from_raw_parts(label, length as usize))
                .map_err(|_| Error::InvalidArgument)?;
            crate::store::validate_volume_label(label).map_err(store_error)?;
            let volume = inner.services.store.create_volume(label, true).map_err(store_error)?;
            let volume = record(volume).expect("new volume label was admitted before commit");
            memory::write_native(output, volume)?;
            Ok(())
        })
    }
}
/// # Safety
/// The handle and disjoint ID/output obey playbit/engine.h.
#[cfg_attr(not(all(target_os = "macos", feature = "macos-shared-runtime")), unsafe(no_mangle))]
pub unsafe extern "C" fn pb_engine_volume_read(
    engine: *mut Engine,
    id: *const [u8; 16],
    output: *mut Volume,
) -> i32 {
    // SAFETY: input is copied before store access, output written only on success.
    unsafe {
        with_engine(engine, |inner| {
            let destination = memory::native_region(output, true)?;
            let source = fixed(id)?;
            disjoint(&destination, &source)?;
            memory::clear_native(output)?;
            let volume =
                inner.services.store.volume(Id::from_bytes(id.read())).map_err(store_error)?;
            memory::write_native(output, record(volume)?)?;
            Ok(())
        })
    }
}
