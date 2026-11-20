//! Audio admission uses the same typed capability authority for both adapters.
//! Device initialization is a pending operation; controls only enqueue local work.
#[cfg(all(feature = "native-audio", target_vendor = "apple"))]
use super::PendingOperation;
use super::{HostError, HostResult, Opcode, OperationHandle, State, require_supported};
#[cfg(all(feature = "native-audio", target_vendor = "apple"))]
use crate::capability::Rights;

pub(in crate::runtime) fn open_begin(state: &mut State, rate: u32) -> HostResult<OperationHandle> {
    require_supported(state, Opcode::AudioOpenBegin)?;
    if rate != 0
        && !(playbit_api::abi::audio::MIN_SAMPLE_RATE..=playbit_api::abi::audio::MAX_SAMPLE_RATE)
            .contains(&rate)
    {
        return Err(HostError::InvalidArgument);
    }
    #[cfg(all(feature = "native-audio", target_vendor = "apple"))]
    {
        if state.work.draining {
            return Err(HostError::Conflict);
        }
        if state.work.pending.keys().any(|raw| {
            matches!(
                state.capabilities.operation(crate::capability::Handle(*raw)),
                Ok(PendingOperation::AudioOpen(_))
            )
        }) {
            return Err(HostError::Conflict);
        }
        require_supported(state, Opcode::AudioOpenTake)?;
        let presentation = state.audio_presentation.clone().ok_or(HostError::Unsupported)?;
        let wake = state.host.audio_wake().ok_or(HostError::Unsupported)?;
        let slot = super::handles::reserve_operation(state)?;
        let pending = match state.audio.begin(&mut state.capabilities, presentation, rate, wake) {
            Ok(pending) => pending,
            Err(error) => {
                super::handles::cancel_operation(state, slot);
                return Err(error);
            }
        };
        super::register_operation(state, slot, PendingOperation::AudioOpen(pending))
    }
    #[cfg(not(all(feature = "native-audio", target_vendor = "apple")))]
    {
        let _ = rate;
        Err(HostError::Unsupported)
    }
}
pub(in crate::runtime) fn open_take(state: &mut State, handle: OperationHandle) -> HostResult<u64> {
    require_supported(state, Opcode::AudioOpenTake)?;
    #[cfg(all(feature = "native-audio", target_vendor = "apple"))]
    {
        let pending = state.capabilities.operation(super::handles::raw_operation(handle))?.clone();
        let PendingOperation::AudioOpen(open) = &pending else {
            return Err(HostError::StaleHandle);
        };
        let result = state.audio.take(&mut state.capabilities, open);
        if result != Err(HostError::WouldBlock) {
            super::consume_operation(state, handle, pending);
        }
        result
    }
    #[cfg(not(all(feature = "native-audio", target_vendor = "apple")))]
    {
        let _ = handle;
        Err(HostError::Unsupported)
    }
}
type GuestCapabilities = crate::runtime::capabilities::Capabilities<super::PendingOperation>;
fn with_audio<T>(
    state: &mut State,
    opcode: Opcode,
    operation: impl FnOnce(&mut AudioTable, &mut GuestCapabilities) -> HostResult<T>,
) -> HostResult<T> {
    require_supported(state, opcode)?;
    #[cfg(all(feature = "native-audio", target_vendor = "apple"))]
    {
        operation(&mut state.audio, &mut state.capabilities)
    }
    #[cfg(not(all(feature = "native-audio", target_vendor = "apple")))]
    {
        let _ = operation;
        Err(HostError::Unsupported)
    }
}
#[cfg(all(feature = "native-audio", target_vendor = "apple"))]
type AudioTable = crate::runtime::audio_handles::Table;
#[cfg(not(all(feature = "native-audio", target_vendor = "apple")))]
struct AudioTable;

// Unsupported builds compile the same public operation signatures. Their calls
// are rejected above before the operation closure can execute.
#[cfg(not(all(feature = "native-audio", target_vendor = "apple")))]
impl AudioTable {
    fn close(&mut self, _: &mut GuestCapabilities, _: u64) -> HostResult<()> {
        Err(HostError::Unsupported)
    }
    fn sample_rate(&mut self, _: &mut GuestCapabilities, _: u64) -> HostResult<u32> {
        Err(HostError::Unsupported)
    }
    fn volume(&mut self, _: &mut GuestCapabilities, _: u64) -> HostResult<f32> {
        Err(HostError::Unsupported)
    }
    fn set_volume(&mut self, _: &mut GuestCapabilities, _: u64, _: f32) -> HostResult<()> {
        Err(HostError::Unsupported)
    }
    fn open_sound(
        &mut self,
        _: &mut GuestCapabilities,
        _: u64,
        _: u32,
        _: u32,
        _: u32,
        _: &[u8],
    ) -> HostResult<u64> {
        Err(HostError::Unsupported)
    }
    fn close_sound(&mut self, _: &mut GuestCapabilities, _: u64) -> HostResult<()> {
        Err(HostError::Unsupported)
    }
}
pub(in crate::runtime) fn close(state: &mut State, audio: u64) -> HostResult<()> {
    with_audio(state, Opcode::AudioClose, |table, caps| table.close(caps, audio))
}
pub(in crate::runtime) fn sample_rate(state: &mut State, audio: u64) -> HostResult<u32> {
    with_audio(state, Opcode::AudioSampleRate, |table, caps| table.sample_rate(caps, audio))
}
pub(in crate::runtime) fn volume(state: &mut State, audio: u64) -> HostResult<f32> {
    with_audio(state, Opcode::AudioVolume, |table, caps| table.volume(caps, audio))
}
pub(in crate::runtime) fn set_volume(state: &mut State, audio: u64, value: f32) -> HostResult<()> {
    with_audio(state, Opcode::AudioSetVolume, |table, caps| table.set_volume(caps, audio, value))
}
pub(in crate::runtime) fn open_pcm(
    state: &mut State,
    audio: u64,
    channels: u32,
    rate: u32,
    frames: u32,
    bytes: &[u8],
) -> HostResult<u64> {
    with_audio(state, Opcode::SoundOpenPcm, |table, caps| {
        table.open_sound(caps, audio, channels, rate, frames, bytes)
    })
}
pub(in crate::runtime) fn sound_close(state: &mut State, sound: u64) -> HostResult<()> {
    with_audio(state, Opcode::SoundClose, |table, caps| table.close_sound(caps, sound))
}
pub(in crate::runtime) fn play(
    state: &mut State,
    sound: u64,
    instance: u32,
    gain: u32,
    start: u64,
    length: u64,
    flags: u32,
) -> HostResult<()> {
    require_supported(state, Opcode::SoundPlay)?;
    #[cfg(all(feature = "native-audio", target_vendor = "apple"))]
    {
        let (mut endpoint, key) = state.audio.sound(&state.capabilities, sound, Rights::WRITE)?;
        endpoint.play(key, instance, gain, start, length, flags)
    }
    #[cfg(not(all(feature = "native-audio", target_vendor = "apple")))]
    {
        let _ = (sound, instance, gain, start, length, flags);
        Err(HostError::Unsupported)
    }
}
pub(in crate::runtime) fn tell(state: &mut State, sound: u64, instance: u32) -> HostResult<u64> {
    require_supported(state, Opcode::SoundTell)?;
    #[cfg(all(feature = "native-audio", target_vendor = "apple"))]
    {
        let (endpoint, key) = state.audio.sound(&state.capabilities, sound, Rights::READ)?;
        endpoint.tell(key, instance)
    }
    #[cfg(not(all(feature = "native-audio", target_vendor = "apple")))]
    {
        let _ = (sound, instance);
        Err(HostError::Unsupported)
    }
}
pub(in crate::runtime) fn seek(
    state: &mut State,
    sound: u64,
    instance: u32,
    position: u64,
) -> HostResult<()> {
    require_supported(state, Opcode::SoundSeek)?;
    #[cfg(all(feature = "native-audio", target_vendor = "apple"))]
    {
        let (mut endpoint, key) = state.audio.sound(&state.capabilities, sound, Rights::WRITE)?;
        endpoint.seek(key, instance, position)
    }
    #[cfg(not(all(feature = "native-audio", target_vendor = "apple")))]
    {
        let _ = (sound, instance, position);
        Err(HostError::Unsupported)
    }
}
pub(in crate::runtime) fn set_gain(
    state: &mut State,
    sound: u64,
    instance: u32,
    gain: u32,
) -> HostResult<()> {
    require_supported(state, Opcode::SoundSetGain)?;
    #[cfg(all(feature = "native-audio", target_vendor = "apple"))]
    {
        let (mut endpoint, key) = state.audio.sound(&state.capabilities, sound, Rights::WRITE)?;
        endpoint.set_gain(key, instance, gain)
    }
    #[cfg(not(all(feature = "native-audio", target_vendor = "apple")))]
    {
        let _ = (sound, instance, gain);
        Err(HostError::Unsupported)
    }
}
pub(in crate::runtime) fn set_balance(
    state: &mut State,
    sound: u64,
    instance: u32,
    balance: f32,
) -> HostResult<()> {
    require_supported(state, Opcode::SoundSetBalance)?;
    #[cfg(all(feature = "native-audio", target_vendor = "apple"))]
    {
        let (mut endpoint, key) = state.audio.sound(&state.capabilities, sound, Rights::WRITE)?;
        endpoint.set_balance(key, instance, balance)
    }
    #[cfg(not(all(feature = "native-audio", target_vendor = "apple")))]
    {
        let _ = (sound, instance, balance);
        Err(HostError::Unsupported)
    }
}
pub(in crate::runtime) fn stop(state: &mut State, sound: u64, instance: u32) -> HostResult<()> {
    require_supported(state, Opcode::SoundStop)?;
    #[cfg(all(feature = "native-audio", target_vendor = "apple"))]
    {
        let (mut endpoint, key) = state.audio.sound(&state.capabilities, sound, Rights::WRITE)?;
        endpoint.stop(key, instance)
    }
    #[cfg(not(all(feature = "native-audio", target_vendor = "apple")))]
    {
        let _ = (sound, instance);
        Err(HostError::Unsupported)
    }
}
#[cfg(all(feature = "native-audio", target_vendor = "apple"))]
pub(in crate::runtime) fn ready(state: &State) -> bool {
    state.work.pending.keys().any(|raw| matches!(state.capabilities.operation(crate::capability::Handle(*raw)),
        Ok(PendingOperation::AudioOpen(open)) if open.ready() && !open.notified.load(std::sync::atomic::Ordering::Acquire)))
}
#[cfg(all(feature = "native-audio", target_vendor = "apple"))]
pub(in crate::runtime) fn pump(state: &mut State) {
    use std::sync::atomic::Ordering;
    let ready: Vec<_> = state
        .work
        .pending
        .iter()
        .filter_map(|(raw, root)| {
            match state.capabilities.operation(crate::capability::Handle(*raw)) {
                Ok(PendingOperation::AudioOpen(open))
                    if open.ready() && !open.notified.load(Ordering::Acquire) =>
                {
                    Some((*raw, *root, open.clone()))
                }
                _ => None,
            }
        })
        .collect();
    for (raw, root, open) in ready {
        if super::events::push_work_event(
            state,
            super::Event {
                kind: super::EventKind::DataReady,
                target: 0,
                data: raw.to_le_bytes().to_vec(),
            },
            Some(root),
        )
        .is_err()
        {
            break;
        }
        open.notified.store(true, Ordering::Release);
    }
}

/// Audio initialization has no durable side effects to drain after retirement.
#[cfg(all(feature = "native-audio", target_vendor = "apple"))]
pub(in crate::runtime) fn cancel_open(state: &mut State) {
    let active = state.work.active;
    let pending: Vec<_> = state
        .work
        .pending
        .keys()
        .filter_map(|raw| {
            let pending = state
                .capabilities
                .operation(crate::capability::Handle(*raw))
                .expect("accepted operation remains owned");
            match pending {
                PendingOperation::AudioOpen(open) => {
                    open.cancel();
                    Some((*raw, pending.clone()))
                }
                _ => None,
            }
        })
        .collect();
    for (raw, pending) in pending {
        super::consume_operation(state, super::handles::operation_handle(raw), pending);
    }
    state.work.active = active;
}
