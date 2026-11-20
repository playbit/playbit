//! Checked PCM spans and full-width scalar capability results.
use super::{WasmState, call};
use crate::runtime::syscall::{Opcode, OperationHandle, audio};
use wasmtime::Caller;
pub(super) fn dispatch(caller: Caller<'_, WasmState>, opcode: Opcode, args: [u64; 6]) -> i64 {
    let [a, b, c, d, e, f] = args;
    call(caller, |state, memory| match opcode {
        Opcode::AudioOpenBegin => {
            let handle = audio::open_begin(state, a as u32)?;
            Ok(playbit_api::abi::dispatch::pack_handle(handle.generation, handle.slot) as i64)
        }
        Opcode::AudioOpenTake => {
            let (generation, slot) = playbit_api::abi::dispatch::unpack_handle(a);
            audio::open_take(state, OperationHandle { generation, slot })
                .map(|handle| handle as i64)
        }
        Opcode::AudioClose => audio::close(state, a).map(|()| 0),
        Opcode::AudioSampleRate => audio::sample_rate(state, a).map(i64::from),
        Opcode::AudioVolume => audio::volume(state, a).map(|volume| i64::from(volume.to_bits())),
        Opcode::AudioSetVolume => audio::set_volume(state, a, f32::from_bits(b as u32)).map(|()| 0),
        Opcode::SoundOpenPcm => {
            let length = playbit_api::abi::audio::pcm_byte_count(b as u32, c as u32, e as u32)?;
            let input = memory.input(
                d as u32,
                length as u32,
                1,
                1,
                playbit_api::abi::audio::MAX_SOUND_BYTES as u32,
                None,
            )?;
            let sound =
                audio::open_pcm(state, a, b as u32, c as u32, e as u32, input.bytes(memory.data))?;
            Ok(sound as i64)
        }
        Opcode::SoundClose => audio::sound_close(state, a).map(|()| 0),
        Opcode::SoundPlay => audio::play(state, a, b as u32, c as u32, d, e, f as u32).map(|()| 0),
        Opcode::SoundTell => {
            let output = memory.admit(c as u32, 1, 8, 8, 1)?;
            let position = audio::tell(state, a, b as u32)?;
            output.write(memory.data, 0, &position.to_le_bytes());
            Ok(0)
        }
        Opcode::SoundSeek => audio::seek(state, a, b as u32, c).map(|()| 0),
        Opcode::SoundSetGain => audio::set_gain(state, a, b as u32, c as u32).map(|()| 0),
        Opcode::SoundSetBalance => {
            audio::set_balance(state, a, b as u32, f32::from_bits(c as u32)).map(|()| 0)
        }
        Opcode::SoundStop => audio::stop(state, a, b as u32).map(|()| 0),
        _ => unreachable!("audio opcode group"),
    })
}
