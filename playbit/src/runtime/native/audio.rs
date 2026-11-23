//! Native audio pointers are borrowed only after exact scalar/span admission.
use super::{State, abi, calls};
use crate::runtime::syscall::{HostResult, Opcode, OperationHandle, audio};
pub(super) unsafe fn dispatch(context: *mut abi::Context, opcode: Opcode, args: [u64; 6]) -> i64 {
    let [a, b, c, d, e, f] = args;
    // SAFETY: scalar admission precedes dispatch; all output spans are admitted
    // before operations that consume pending results or allocate resources.
    unsafe {
        calls::with_context(context, |state| match opcode {
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
            Opcode::AudioVolume => {
                audio::volume(state, a).map(|volume| i64::from(volume.to_bits()))
            }
            Opcode::AudioSetVolume => {
                audio::set_volume(state, a, f32::from_bits(b as u32)).map(|()| 0)
            }
            Opcode::SoundOpenPcm => open_pcm(state, args),
            Opcode::SoundClose => audio::sound_close(state, a).map(|()| 0),
            Opcode::SoundPlay => {
                audio::play(state, a, b as u32, c as u32, d, e, f as u32).map(|()| 0)
            }
            Opcode::SoundTell => {
                let output = c as usize as *mut u64;
                calls::span(output, 1)?;
                output.write(audio::tell(state, a, b as u32)?);
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
}

unsafe fn open_pcm(state: &mut State, [a, b, c, d, e, _]: [u64; 6]) -> HostResult<i64> {
    let length = playbit_api::abi::audio::pcm_byte_count(b as u32, c as u32, e as u32)?;
    let source = d as usize as *const u8;
    // SAFETY: structural admission bounds the readable PCM input; allocation
    // validity and lifetime are the native caller's borrowing contract.
    unsafe {
        let samples =
            calls::bytes(source, length as u32, playbit_api::abi::audio::MAX_SOUND_BYTES)?;
        audio::open_pcm(state, a, b as u32, c as u32, e as u32, samples).map(|handle| handle as i64)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests use valid owned native fixture allocations.")]
mod tests {
    use crate::runtime::native::{AtomicBool, Context, Mutex};
    #[test]
    fn native_audio_admits_pcm_before_effects() {
        // Invalid memory/format is rejected before unsupported host admission;
        // the borrowed source stays intact on failure.
        let (state, _, _) = crate::runtime::syscall::pending_data::tests::state();
        let context = Box::new(Context {
            state: Mutex::new(state),
            owner: std::thread::current().id(),
            active: AtomicBool::new(true),
            poll_yielder: Default::default(),
        });
        let raw = (&*context as *const Context).cast_mut().cast();
        let mut storage = [0xaaaaaaaaaaaaaaaa_u64; 4];
        let base = storage.as_mut_ptr() as usize as u64;
        for (source, channels, frames, expected) in
            [(0, 1, 1, -3), (base + 8, 2, u32::MAX, -9), (base + 8, 0, 1, -3), (base + 8, 1, 1, -7)]
        {
            // SAFETY: actual spans are owned fixture storage; excessive sizes
            // and malformed pointers are rejected before dereferencing input.
            let actual = unsafe {
                crate::runtime::native::dispatch::pb_syscall(
                    raw,
                    0x1a6,
                    0,
                    channels,
                    44_100,
                    source,
                    frames as u64,
                    0,
                )
            };
            assert_eq!(actual, expected);
            assert_eq!(storage, [0xaaaaaaaaaaaaaaaa; 4]);
        }
    }
    #[cfg(all(feature = "native-audio", target_vendor = "apple"))]
    #[test]
    fn native_audio_dispatch_owns_resources_and_controls_independent_instances() {
        // Exercise the real native scalar adapter and capability admission with
        // an initialized-result fixture that never initializes an output device.
        use crate::runtime::{audio, audio_handles};
        use playbit_api::abi::{audio as contract, dispatch::pack_handle, syscall::Opcode};
        let _serial = audio::TEST_SERIAL.lock().unwrap();
        let (mut state, _, _) = crate::runtime::syscall::pending_data::tests::state();
        let operation = audio_handles::prepare_open(&mut state, 44_100).unwrap();
        let operation = pack_handle(operation.generation, operation.slot);
        let context = Box::new(Context {
            state: Mutex::new(state),
            owner: std::thread::current().id(),
            active: AtomicBool::new(true),
            poll_yielder: Default::default(),
        });
        let call = |opcode: Opcode, [a, b, c, d, e, f]: [u64; 6]| {
            let raw = (&*context as *const Context).cast_mut().cast();
            // SAFETY: pointer arguments borrow fixture allocations or malformed
            // spans explicitly rejected before any dereference.
            unsafe {
                crate::runtime::native::dispatch::pb_syscall(raw, opcode as u32, a, b, c, d, e, f)
            }
        };
        let mut storage = [0xaaaaaaaaaaaaaaaa_u64; 4];
        let base = storage.as_mut_ptr() as usize as u64;
        assert_eq!(call(Opcode::AudioOpenTake, [operation, base + 1, 0, 0, 0, 0]), -3);
        assert_eq!(storage[0], 0xaaaaaaaaaaaaaaaa);
        let output = call(Opcode::AudioOpenTake, [operation, 0, 0, 0, 0, 0]);
        assert!(output > 0);
        let output = output as u64;
        assert_eq!(call(Opcode::AudioOpenTake, [operation, 0, 0, 0, 0, 0]), -8);
        assert_eq!(storage[0], 0xaaaaaaaaaaaaaaaa);
        assert_eq!(call(Opcode::AudioSampleRate, [output, 0, 0, 0, 0, 0]), 44_100);
        assert_eq!(
            call(Opcode::AudioSetVolume, [output, (-0.0_f32).to_bits() as u64, 0, 0, 0, 0]),
            0
        );
        assert_eq!(call(Opcode::AudioVolume, [output, 0, 0, 0, 0, 0]), 0);
        assert_eq!(call(Opcode::AudioSetVolume, [output, 0.5_f32.to_bits() as u64, 0, 0, 0, 0]), 0);
        assert_eq!(call(Opcode::AudioVolume, [output, 0, 0, 0, 0, 0]), 0.5_f32.to_bits() as i64);
        assert_eq!(
            call(Opcode::AudioSetVolume, [output, f32::NAN.to_bits() as u64, 0, 0, 0, 0]),
            -3
        );
        let sound = call(Opcode::SoundOpenPcm, [output, 2, 44_100, base + 16, 4, 0]);
        assert!(sound > 0);
        let sound = sound as u64;
        assert_eq!(call(Opcode::SoundOpenPcm, [sound, 2, 44_100, base + 16, 4, 0]), -8);
        assert_eq!(storage[1], 0xaaaaaaaaaaaaaaaa);
        assert_eq!(call(Opcode::AudioSampleRate, [sound, 0, 0, 0, 0, 0]), -8);
        for id in [1, 1234, 0x80000000] {
            assert_eq!(
                call(
                    Opcode::SoundPlay,
                    [
                        sound,
                        id,
                        contract::UNITY_GAIN as u64,
                        0,
                        90_703,
                        contract::SOUND_LOOP as u64
                    ]
                ),
                0
            );
        }
        for length in [0, 1] {
            assert_eq!(call(Opcode::SoundPlay, [sound, 2, 65_536, 0, length, 0]), -3);
        }
        assert_eq!(call(Opcode::SoundPlay, [sound, 2, 65_536, 0, 90_703, 2]), -3);
        assert_eq!(call(Opcode::SoundSetGain, [sound, 1, 16_384, 0, 0, 0]), 0);
        assert_eq!(
            call(Opcode::SoundSetBalance, [sound, 1, (-0.5_f32).to_bits() as u64, 0, 0, 0]),
            0
        );
        assert_eq!(call(Opcode::SoundSeek, [sound, 1, 45_352, 0, 0, 0]), 0);
        assert_eq!(call(Opcode::SoundTell, [sound, 1, base, 0, 0, 0]), 0);
        assert_eq!(storage[0], 45_352);
        assert_eq!(call(Opcode::SoundTell, [sound, 1234, base, 0, 0, 0]), 0);
        assert_eq!(storage[0], 0);
        assert_eq!(call(Opcode::SoundSeek, [sound, 1, 90_703, 0, 0, 0]), 0);
        assert_eq!(call(Opcode::SoundTell, [sound, 1, base, 0, 0, 0]), 0);
        assert_eq!(storage[0], 90_703);
        assert_eq!(call(Opcode::SoundSeek, [sound, 1, 113_379, 0, 0, 0]), -3);
        assert_eq!(call(Opcode::SoundSetGain, [sound, 1, 65_537, 0, 0, 0]), -3);
        assert_eq!(
            call(Opcode::SoundSetBalance, [sound, 1, f32::NAN.to_bits() as u64, 0, 0, 0]),
            -3
        );
        storage[0] = 0xaaaaaaaaaaaaaaaa;
        assert_eq!(call(Opcode::SoundTell, [sound, 1, base + 1, 0, 0, 0]), -3);
        assert_eq!(call(Opcode::SoundTell, [sound, 456, base, 0, 0, 0]), -3);
        assert_eq!(call(Opcode::SoundTell, [output, 1, base, 0, 0, 0]), -8);
        assert_eq!(storage[0], 0xaaaaaaaaaaaaaaaa);
        for id in [1, 1234] {
            assert_eq!(call(Opcode::SoundStop, [sound, id, 0, 0, 0, 0]), 0);
            assert_eq!(call(Opcode::SoundTell, [sound, id, base, 0, 0, 0]), -3);
        }
        assert_eq!(call(Opcode::SoundClose, [sound, 0, 0, 0, 0, 0]), 0);
        assert_eq!(call(Opcode::SoundTell, [sound, 0x80000000, base, 0, 0, 0]), -8);
        let child = call(Opcode::SoundOpenPcm, [output, 2, 44_100, base + 16, 4, 0]);
        assert!(child > 0);
        let child = child as u64;
        assert_ne!(child, sound);
        assert_eq!(call(Opcode::SoundPlay, [child, 1234, 65_536, 0, 90_703, 0]), 0);
        assert_eq!(call(Opcode::AudioClose, [output, 0, 0, 0, 0, 0]), 0);
        assert_eq!(call(Opcode::AudioSampleRate, [output, 0, 0, 0, 0, 0]), -8);
        assert_eq!(call(Opcode::AudioVolume, [output, 0, 0, 0, 0, 0]), -8);
        assert_eq!(call(Opcode::SoundTell, [child, 1234, base, 0, 0, 0]), -8);
        assert_eq!(call(Opcode::SoundClose, [child, 0, 0, 0, 0, 0]), -8);
        assert_eq!(storage[0], 0xaaaaaaaaaaaaaaaa);
    }
}
