//! Mono PCM16LE notes. The recurrence needs trigonometry only once per note.
use std::f64::consts::TAU;

pub const SAMPLE_RATE: u32 = 48_000;
pub const FRAMES: usize = SAMPLE_RATE as usize / 2;
pub const DURATION_NS: u64 = 500_000_000;
pub const NAMES: [&str; 8] = ["C4", "D4", "E4", "F4", "G4", "A4", "B4", "C5"];
const FREQUENCIES: [f64; 8] = [261.63, 293.66, 329.63, 349.23, 392.0, 440.0, 493.88, 523.25];

pub fn note(index: usize) -> Vec<u8> {
    let (step_sin, step_cos) = (TAU * FREQUENCIES[index] / f64::from(SAMPLE_RATE)).sin_cos();
    let (mut sine, mut cosine) = (0.0, 1.0);
    let mut pcm = Vec::with_capacity(FRAMES * 2);
    for frame in 0..FRAMES {
        let time = frame as f64 / f64::from(SAMPLE_RATE);
        let remaining = (FRAMES - 1 - frame) as f64 / f64::from(SAMPLE_RATE);
        let envelope = (time / 0.008).min(1.0) * (remaining / 0.12).min(1.0) * (1.0 - time * 1.4);
        // Eight full-volume notes fit inside PCM16's amplitude range.
        let sample = (sine * envelope * 0.12 * f64::from(i16::MAX)).round() as i16;
        pcm.extend_from_slice(&sample.to_le_bytes());
        (sine, cosine) = (sine * step_cos + cosine * step_sin, cosine * step_cos - sine * step_sin);
    }
    pcm
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notes_have_their_pitch_duration_and_click_free_envelope() {
        for (index, frequency) in FREQUENCIES.into_iter().enumerate() {
            let pcm = note(index);
            let samples: Vec<_> =
                pcm.as_chunks::<2>().0.iter().copied().map(i16::from_le_bytes).collect();
            assert_eq!(samples.len(), FRAMES);
            assert_eq!(samples[0], 0);
            assert_eq!(samples[FRAMES - 1], 0);
            assert!(samples.iter().all(|sample| sample.unsigned_abs() <= 3933));
            let crossings = samples.windows(2).filter(|pair| pair[0] <= 0 && pair[1] > 0).count();
            assert!((crossings as f64 - frequency * 0.5).abs() < 2.0);
            assert!(
                samples[FRAMES / 3..FRAMES / 2].iter().any(|sample| sample.unsigned_abs() > 1000)
            );
            assert_eq!(DURATION_NS * u64::from(SAMPLE_RATE) / 1_000_000_000, FRAMES as u64);
        }
    }
}
