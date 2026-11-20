//! Native presentation lifecycle for generation-scoped guest audio.
use playbit_api::abi::syscall::Error;

/// # Safety
/// `text` points to `length` live UTF-8 bytes, or is null when length is zero.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pb_ios_audio_active(text: *const u8, length: usize, active: u8) -> i32 {
    if text.is_null() || length == 0 || length > 22 || active > 1 {
        return Error::InvalidArgument as i32;
    }
    // SAFETY: the caller retains the bounded input for this synchronous call.
    let bytes = unsafe { std::slice::from_raw_parts(text, length) };
    let Ok(text) = std::str::from_utf8(bytes) else { return Error::InvalidArgument as i32 };
    let Ok(instance) = crate::identifier::decode::<16>(text) else {
        return Error::InvalidArgument as i32;
    };
    crate::runtime::audio::set_instance_active(instance, active != 0);
    0
}

/// Native app suspension and AVAudioSession interruptions stop every voice.
#[unsafe(no_mangle)]
pub extern "C" fn pb_ios_audio_foreground(active: u8) {
    crate::runtime::audio::set_all_active(active == 1);
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn visibility_rejects_malformed_identifiers_before_reading() {
        assert_eq!(
            unsafe { pb_ios_audio_active(std::ptr::null(), 1, 1) },
            Error::InvalidArgument as i32
        );
        let invalid = b"not a canonical identifier";
        assert_eq!(
            unsafe { pb_ios_audio_active(invalid.as_ptr(), invalid.len(), 1) },
            Error::InvalidArgument as i32
        );
        let id = crate::identifier::encode(&[1; 16]);
        assert_eq!(
            unsafe { pb_ios_audio_active(id.as_ptr(), id.len(), 2) },
            Error::InvalidArgument as i32
        );
        assert_eq!(unsafe { pb_ios_audio_active(id.as_ptr(), id.len(), 1) }, 0);
    }
}
