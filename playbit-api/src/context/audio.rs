//! Safe PCM audio resources. Handles belong to the current guest capability table.
use super::{Completion, Context, Error, OperationHandle, status};
use crate::abi::audio as ffi;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Audio(u64);
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Sound(u64);
/// A caller-chosen playback identifier within one sound.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SoundInstance {
    pub sound: Sound,
    pub instance: u32,
}
/// Source sound time in nanoseconds. Inputs round down to a source PCM frame;
/// rendered cursor results round up to a whole nanosecond for seek round trips.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Timestamp(pub u64);
/// Fixed point amplitude: 0 is silent; 65,536 preserves original amplitude.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Gain(pub u32);

impl Context<'_> {
    pub fn begin_open_audio(
        &mut self,
        preferred_sample_rate: u32,
    ) -> Result<OperationHandle, Error> {
        // SAFETY: context is borrowed from the active app callback.
        let result = unsafe { ffi::pb_audio_open_begin(self.raw.as_ptr(), preferred_sample_rate) };
        super::pending::admitted(result)
    }
    pub fn take_open_audio(
        &mut self,
        operation: OperationHandle,
    ) -> Result<Completion<Audio>, Error> {
        // SAFETY: context is borrowed from the active app callback.
        let code = unsafe { ffi::pb_audio_open_take(self.raw.as_ptr(), operation) };
        if code == Error::WouldBlock as i64 {
            return Ok(Completion::Pending);
        }
        Ok(Completion::Ready(Audio(super::pending::handle_result(code)?)))
    }
    pub fn audio_close(&mut self, audio: Audio) -> Result<(), Error> {
        // SAFETY: context is borrowed from the active app callback.
        status(unsafe { ffi::pb_audio_close(self.raw.as_ptr(), audio.0) })
    }
    pub fn audio_sample_rate(&mut self, audio: Audio) -> Result<u32, Error> {
        // SAFETY: context is borrowed from the active app callback.
        query(unsafe { ffi::pb_audio_sample_rate(self.raw.as_ptr(), audio.0) })
    }
    pub fn audio_volume(&mut self, audio: Audio) -> Result<f32, Error> {
        // SAFETY: context is borrowed from the active app callback.
        query(unsafe { ffi::pb_audio_volume(self.raw.as_ptr(), audio.0) }).map(f32::from_bits)
    }
    pub fn audio_set_volume(&mut self, audio: Audio, volume: f32) -> Result<(), Error> {
        // SAFETY: context is live; the host validates the scalar value.
        status(unsafe { ffi::pb_audio_set_volume(self.raw.as_ptr(), audio.0, volume) })
    }
    pub fn sound_open_pcm(
        &mut self,
        audio: Audio,
        channel_count: u32,
        sample_rate: u32,
        pcm16_le: &[u8],
    ) -> Result<Sound, Error> {
        let frame_bytes = channel_count.checked_mul(2).ok_or(Error::InvalidArgument)? as usize;
        if frame_bytes == 0 || pcm16_le.is_empty() || !pcm16_le.len().is_multiple_of(frame_bytes) {
            return Err(Error::InvalidArgument);
        }
        let frame_count =
            u32::try_from(pcm16_le.len() / frame_bytes).map_err(|_| Error::LimitExceeded)?;
        ffi::pcm_byte_count(channel_count, sample_rate, frame_count)?;
        // SAFETY: the borrowed PCM input remains readable and stable for this call.
        let result = unsafe {
            ffi::pb_sound_open_pcm(
                self.raw.as_ptr(),
                audio.0,
                channel_count,
                sample_rate,
                pcm16_le.as_ptr(),
                frame_count,
            )
        };
        Ok(Sound(super::pending::handle_result(result)?))
    }
    pub fn sound_close(&mut self, sound: Sound) -> Result<(), Error> {
        // SAFETY: context is borrowed from the active app callback.
        status(unsafe { ffi::pb_sound_close(self.raw.as_ptr(), sound.0) })
    }
    pub fn sound_play(
        &mut self,
        sound: SoundInstance,
        gain: Gain,
        start: Timestamp,
        length: Timestamp,
        flags: u32,
    ) -> Result<(), Error> {
        // SAFETY: context is live; the host validates all scalar arguments.
        status(unsafe {
            ffi::pb_sound_play(
                self.raw.as_ptr(),
                sound.sound.0,
                sound.instance,
                gain.0,
                start.0,
                length.0,
                flags,
            )
        })
    }
    pub fn sound_tell(&mut self, sound: SoundInstance) -> Result<Timestamp, Error> {
        let mut output = 0;
        // SAFETY: one initialized timestamp is exclusively borrowed.
        status(unsafe {
            ffi::pb_sound_tell(self.raw.as_ptr(), sound.sound.0, sound.instance, &mut output)
        })?;
        Ok(Timestamp(output))
    }
    pub fn sound_seek(&mut self, sound: SoundInstance, position: Timestamp) -> Result<(), Error> {
        // SAFETY: context is live; the host validates the timestamp.
        status(unsafe {
            ffi::pb_sound_seek(self.raw.as_ptr(), sound.sound.0, sound.instance, position.0)
        })
    }
    pub fn sound_set_gain(&mut self, sound: SoundInstance, gain: Gain) -> Result<(), Error> {
        // SAFETY: context is live; the host validates the gain.
        status(unsafe {
            ffi::pb_sound_set_gain(self.raw.as_ptr(), sound.sound.0, sound.instance, gain.0)
        })
    }
    pub fn sound_set_balance(&mut self, sound: SoundInstance, balance: f32) -> Result<(), Error> {
        // SAFETY: context is live; the host validates the balance.
        status(unsafe {
            ffi::pb_sound_set_balance(self.raw.as_ptr(), sound.sound.0, sound.instance, balance)
        })
    }
    pub fn sound_stop(&mut self, sound: SoundInstance) -> Result<(), Error> {
        // SAFETY: context is live; the host validates the instance.
        status(unsafe { ffi::pb_sound_stop(self.raw.as_ptr(), sound.sound.0, sound.instance) })
    }
}

fn query(value: i64) -> Result<u32, Error> {
    if value < 0 {
        return Err(i32::try_from(value)
            .ok()
            .and_then(|code| Error::try_from(code).ok())
            .unwrap_or(Error::Internal));
    }
    u32::try_from(value).map_err(|_| Error::Internal)
}
