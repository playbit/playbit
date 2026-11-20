//! PCM audio resources and independent sound instances.
#![allow(unsafe_code, reason = "Typed wrappers lower borrowed buffers to the scalar ABI.")]
use super::{
    operations::Context,
    pending::OperationHandle,
    syscall::{Error, Opcode},
};

pub const DEFAULT_SAMPLE_RATE: u32 = 48_000;
pub const MIN_SAMPLE_RATE: u32 = 8_000;
pub const MAX_SAMPLE_RATE: u32 = 192_000;
pub const MAX_CHANNELS: u32 = 2;
pub const MAX_SOUNDS: usize = 16;
pub const MAX_PLAYBACK_INSTANCES: usize = 8;
pub const MAX_SOUND_BYTES: usize = 1024 * 1024;
pub const MAX_COMMANDS: usize = 32;
/// Amplitude multiplier is gain / 65,536. This value preserves original amplitude.
pub const UNITY_GAIN: u32 = 65_536;
pub const SOUND_LOOP: u32 = 1;

pub const OPCODES: [Opcode; 14] = [
    Opcode::AudioOpenBegin,
    Opcode::AudioOpenTake,
    Opcode::AudioClose,
    Opcode::AudioSampleRate,
    Opcode::AudioVolume,
    Opcode::AudioSetVolume,
    Opcode::SoundOpenPcm,
    Opcode::SoundClose,
    Opcode::SoundPlay,
    Opcode::SoundTell,
    Opcode::SoundSeek,
    Opcode::SoundSetGain,
    Opcode::SoundSetBalance,
    Opcode::SoundStop,
];

/// Validate source format and bound the borrowed PCM span before accessing it.
pub fn pcm_byte_count(
    channel_count: u32,
    sample_rate: u32,
    frame_count: u32,
) -> Result<usize, Error> {
    if !(1..=MAX_CHANNELS).contains(&channel_count)
        || !(MIN_SAMPLE_RATE..=MAX_SAMPLE_RATE).contains(&sample_rate)
        || frame_count == 0
    {
        return Err(Error::InvalidArgument);
    }
    (frame_count as usize)
        .checked_mul(channel_count as usize)
        .and_then(|samples| samples.checked_mul(2))
        .filter(|bytes| *bytes <= MAX_SOUND_BYTES)
        .ok_or(Error::LimitExceeded)
}

/// # Safety
/// Context is borrowed from the active app callback.
pub unsafe fn pb_audio_open_begin(context: *mut Context, preferred_sample_rate: u32) -> i64 {
    // SAFETY: the caller supplies the context and borrowed spans required by this operation.
    unsafe {
        super::invocation::invoke(
            context,
            Opcode::AudioOpenBegin as u32,
            [preferred_sample_rate as u64, 0, 0, 0, 0, 0],
        )
    }
}

/// # Safety
/// Context is borrowed from the active app callback.
pub unsafe fn pb_audio_open_take(context: *mut Context, operation: OperationHandle) -> i64 {
    // SAFETY: the caller supplies the context and borrowed spans required by this operation.
    unsafe {
        super::invocation::invoke(
            context,
            Opcode::AudioOpenTake as u32,
            [operation.0, 0, 0, 0, 0, 0],
        )
    }
}

/// # Safety
/// Context is borrowed from the active app callback.
pub unsafe fn pb_audio_close(context: *mut Context, audio: u64) -> i64 {
    // SAFETY: the caller supplies the context and borrowed spans required by this operation.
    unsafe { super::invocation::invoke(context, Opcode::AudioClose as u32, [audio, 0, 0, 0, 0, 0]) }
}

/// # Safety
/// Context is borrowed from the active app callback.
pub unsafe fn pb_audio_sample_rate(context: *mut Context, audio: u64) -> i64 {
    // SAFETY: the caller supplies the context and borrowed spans required by this operation.
    unsafe {
        super::invocation::invoke(context, Opcode::AudioSampleRate as u32, [audio, 0, 0, 0, 0, 0])
    }
}

/// # Safety
/// Context is borrowed from the active app callback.
pub unsafe fn pb_audio_volume(context: *mut Context, audio: u64) -> i64 {
    // SAFETY: the caller supplies the context and borrowed spans required by this operation.
    unsafe {
        super::invocation::invoke(context, Opcode::AudioVolume as u32, [audio, 0, 0, 0, 0, 0])
    }
}

/// # Safety
/// Context is borrowed from the active app callback.
pub unsafe fn pb_audio_set_volume(context: *mut Context, audio: u64, volume: f32) -> i64 {
    // SAFETY: the caller supplies the context and borrowed spans required by this operation.
    unsafe {
        super::invocation::invoke(
            context,
            Opcode::AudioSetVolume as u32,
            [audio, volume.to_bits() as u64, 0, 0, 0, 0],
        )
    }
}

/// # Safety
/// Context is live; samples contains frame_count * channel_count * 2 PCM16LE
/// bytes that remain readable and stable for this call.
/// No caller pointer is retained.
pub unsafe fn pb_sound_open_pcm(
    context: *mut Context,
    audio: u64,
    channel_count: u32,
    sample_rate: u32,
    samples: *const u8,
    frame_count: u32,
) -> i64 {
    // SAFETY: the caller supplies the context and borrowed spans required by this operation.
    unsafe {
        super::invocation::invoke(
            context,
            Opcode::SoundOpenPcm as u32,
            [
                audio,
                channel_count as u64,
                sample_rate as u64,
                samples as usize as u64,
                frame_count as u64,
                0,
            ],
        )
    }
}

/// # Safety
/// Context is borrowed from the active app callback.
pub unsafe fn pb_sound_close(context: *mut Context, sound: u64) -> i64 {
    // SAFETY: the caller supplies the context and borrowed spans required by this operation.
    unsafe { super::invocation::invoke(context, Opcode::SoundClose as u32, [sound, 0, 0, 0, 0, 0]) }
}

/// # Safety
/// Context is borrowed from the active app callback.
pub unsafe fn pb_sound_play(
    context: *mut Context,
    sound: u64,
    instance: u32,
    gain: u32,
    start: u64,
    length: u64,
    flags: u32,
) -> i64 {
    // SAFETY: the caller supplies the context and borrowed spans required by this operation.
    unsafe {
        super::invocation::invoke(
            context,
            Opcode::SoundPlay as u32,
            [sound, instance as u64, gain as u64, start, length, flags as u64],
        )
    }
}

/// # Safety
/// Context is live; output is one aligned exclusive timestamp.
pub unsafe fn pb_sound_tell(
    context: *mut Context,
    sound: u64,
    instance: u32,
    output: *mut u64,
) -> i64 {
    // SAFETY: the caller supplies the context and borrowed spans required by this operation.
    unsafe {
        super::invocation::invoke(
            context,
            Opcode::SoundTell as u32,
            [sound, instance as u64, output as usize as u64, 0, 0, 0],
        )
    }
}

/// # Safety
/// Context is borrowed from the active app callback.
pub unsafe fn pb_sound_seek(
    context: *mut Context,
    sound: u64,
    instance: u32,
    position: u64,
) -> i64 {
    // SAFETY: the caller supplies the context and borrowed spans required by this operation.
    unsafe {
        super::invocation::invoke(
            context,
            Opcode::SoundSeek as u32,
            [sound, instance as u64, position, 0, 0, 0],
        )
    }
}

/// # Safety
/// Context is borrowed from the active app callback.
pub unsafe fn pb_sound_set_gain(
    context: *mut Context,
    sound: u64,
    instance: u32,
    gain: u32,
) -> i64 {
    // SAFETY: the caller supplies the context and borrowed spans required by this operation.
    unsafe {
        super::invocation::invoke(
            context,
            Opcode::SoundSetGain as u32,
            [sound, instance as u64, gain as u64, 0, 0, 0],
        )
    }
}

/// # Safety
/// Context is borrowed from the active app callback.
pub unsafe fn pb_sound_set_balance(
    context: *mut Context,
    sound: u64,
    instance: u32,
    balance: f32,
) -> i64 {
    // SAFETY: the caller supplies the context and borrowed spans required by this operation.
    unsafe {
        super::invocation::invoke(
            context,
            Opcode::SoundSetBalance as u32,
            [sound, instance as u64, balance.to_bits() as u64, 0, 0, 0],
        )
    }
}

/// # Safety
/// Context is borrowed from the active app callback.
pub unsafe fn pb_sound_stop(context: *mut Context, sound: u64, instance: u32) -> i64 {
    // SAFETY: the caller supplies the context and borrowed spans required by this operation.
    unsafe {
        super::invocation::invoke(
            context,
            Opcode::SoundStop as u32,
            [sound, instance as u64, 0, 0, 0, 0],
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pcm_spans_count_interleaved_frames_and_reject_invalid_formats() {
        // Format admission bounds memory reads independently of host availability.
        assert_eq!(pcm_byte_count(1, MIN_SAMPLE_RATE, 1), Ok(2));
        assert_eq!(pcm_byte_count(2, MAX_SAMPLE_RATE, 32), Ok(128));
        assert_eq!(pcm_byte_count(2, 44_100, (MAX_SOUND_BYTES / 4) as u32), Ok(MAX_SOUND_BYTES));
        assert_eq!(
            pcm_byte_count(2, 48_000, (MAX_SOUND_BYTES / 4 + 1) as u32),
            Err(Error::LimitExceeded)
        );
        assert_eq!(pcm_byte_count(2, 48_000, u32::MAX), Err(Error::LimitExceeded));
        for channels in [0, MAX_CHANNELS + 1, u32::MAX] {
            assert_eq!(pcm_byte_count(channels, 48_000, 1), Err(Error::InvalidArgument));
        }
        for rate in [0, MIN_SAMPLE_RATE - 1, MAX_SAMPLE_RATE + 1, u32::MAX] {
            assert_eq!(pcm_byte_count(1, rate, 1), Err(Error::InvalidArgument));
        }
        assert_eq!(pcm_byte_count(1, 48_000, 0), Err(Error::InvalidArgument));
    }
}
