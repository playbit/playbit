//! Callback-owned playback mixer. Rendering neither allocates nor locks;
//! replaced references return through the producer's reclamation queue.
use super::{Arc, Command, Ordering, Playback, Shared, abi};
struct Instance {
    playback: Arc<Playback>,
    phase: u64,
    seek_sequence: u32,
    epoch: u64,
    looping: bool,
    playing: bool,
    gain: u32,
    balance: [u32; 2],
}
pub(super) struct Mixer {
    instances: [Option<Instance>; abi::MAX_PLAYBACK_INSTANCES],
}
impl Mixer {
    pub(super) fn new() -> Self {
        Self { instances: std::array::from_fn(|_| None) }
    }
    fn stop(&mut self, slot: usize, shared: &Shared) {
        if let Some(value) = self.instances[slot].take() {
            shared.retire(value.playback);
        }
    }
    fn suspend(&mut self, shared: &Shared, epoch: u64, active: bool) {
        let halted = shared.halted.load(Ordering::Acquire);
        for slot in 0..self.instances.len() {
            if halted {
                self.stop(slot, shared);
            } else if let Some(instance) = &mut self.instances[slot]
                && !instance.looping
                && (instance.epoch != epoch || !active)
            {
                instance.playing = false;
            }
        }
    }
    pub(super) fn render(&mut self, shared: &Shared, output: &mut [i16]) -> bool {
        assert!(output.len().is_multiple_of(2), "stereo output contains complete frames");
        for _ in 0..abi::MAX_COMMANDS {
            let Some(command) = shared.commands.pop() else { break };
            self.command(command, shared);
        }
        let (active, epoch) = shared.lifecycle();
        self.suspend(shared, epoch, active);
        let playing = self.instances.iter().flatten().any(|instance| instance.playing);
        if active {
            let volume = f32::from_bits(shared.volume.load(Ordering::Acquire));
            let gain = (volume * abi::UNITY_GAIN as f32) as u32;
            for frame in output.as_chunks_mut::<2>().0 {
                let mut mixed = [0_i64; 2];
                for instance in self.instances.iter_mut().flatten() {
                    if instance.playing {
                        mix_frame(instance, shared.sample_rate, &mut mixed);
                    }
                }
                for channel in 0..2 {
                    let sample = mixed[channel] * i64::from(gain) / i64::from(abi::UNITY_GAIN);
                    frame[channel] = sample.clamp(i64::from(i16::MIN), i64::from(i16::MAX)) as i16;
                }
            }
        } else {
            output.fill(0);
        }
        // Publish future work, including a resumed one-shot consumed without
        // output. Paused loops retain readiness until visibility permits output.
        shared.playing.store(
            self.instances.iter().flatten().any(|instance| instance.playing),
            Ordering::Release,
        );
        // A final one-shot buffer still contains PCM even when readiness clears.
        active && playing
    }
    pub(super) fn command(&mut self, command: Command, shared: &Shared) {
        match command {
            Command::Play { slot, playback, looping, gain, epoch } => {
                self.stop(slot, shared);
                let phase = u64::from(playback.start) * u64::from(shared.sample_rate);
                self.instances[slot] = Some(Instance {
                    playback,
                    phase,
                    seek_sequence: 0,
                    epoch,
                    looping,
                    playing: true,
                    gain,
                    balance: [abi::UNITY_GAIN; 2],
                });
            }
            Command::Stop(slot) => self.stop(slot, shared),
            Command::Gain { slot, gain } => {
                if let Some(instance) = &mut self.instances[slot] {
                    instance.gain = gain;
                }
            }
            Command::Balance { slot, left, right } => {
                if let Some(instance) = &mut self.instances[slot] {
                    instance.balance = [left, right];
                }
            }
            Command::Seek { slot, frame, sequence } => {
                if let Some(instance) = &mut self.instances[slot] {
                    instance.phase = u64::from(frame) * u64::from(shared.sample_rate);
                    instance.playing = frame < instance.playback.end;
                    instance.seek_sequence = sequence;
                    publish_position(instance, u64::from(frame));
                }
            }
            Command::Close(key) => {
                for slot in 0..self.instances.len() {
                    if self.instances[slot].as_ref().is_some_and(|i| i.playback.sound.key == key) {
                        self.stop(slot, shared);
                    }
                }
            }
        }
    }
}
fn mix_frame(instance: &mut Instance, output_rate: u32, mixed: &mut [i64; 2]) {
    let sound = &instance.playback.sound;
    let rate = u64::from(output_rate);
    let frame = (instance.phase / rate) as usize;
    let fraction = (instance.phase % rate) as i64;
    let next = if frame + 1 < instance.playback.end as usize {
        frame + 1
    } else if instance.looping {
        instance.playback.start as usize
    } else {
        frame
    };
    let channels = sound.channels as usize;
    for (channel, mixed_sample) in mixed.iter_mut().enumerate() {
        let channel_index = if channels == 1 { 0 } else { channel };
        let first = i64::from(sound.samples[frame * channels + channel_index]);
        let second = i64::from(sound.samples[next * channels + channel_index]);
        let sample = (first * (i64::from(output_rate) - fraction) + second * fraction)
            / i64::from(output_rate);
        let sample = sample * i64::from(instance.gain) / i64::from(abi::UNITY_GAIN);
        *mixed_sample += sample * i64::from(instance.balance[channel]) / i64::from(abi::UNITY_GAIN);
    }
    instance.phase += u64::from(sound.sample_rate);
    let end = u64::from(instance.playback.end) * rate;
    if instance.phase >= end {
        if instance.looping {
            let start = u64::from(instance.playback.start) * rate;
            instance.phase = start + (instance.phase - start) % (end - start);
        } else {
            instance.phase = end;
            instance.playing = false;
        }
    }
    publish_position(instance, instance.phase / rate);
}
fn publish_position(instance: &Instance, frame: u64) {
    let position = (u64::from(instance.seek_sequence) << 32) | frame;
    instance.playback.position.store(position, Ordering::Release);
}
