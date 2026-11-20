//! Admission, DSP and lifecycle checks run the real mixer without opening output.
#![allow(clippy::unwrap_used, reason = "Tests assert valid fixture operations.")]
use super::*;
fn endpoint(rate: u32) -> Endpoint {
    let endpoint = Endpoint::new(Presentation::new([91; 16]), rate).unwrap();
    endpoint.shared.presentation.activate(true);
    endpoint
}
fn pcm(samples: &[i16]) -> Vec<u8> {
    samples.iter().flat_map(|sample| sample.to_le_bytes()).collect()
}
fn timestamp(frames: u64, rate: u32) -> u64 {
    (frames * NANOSECONDS).div_ceil(u64::from(rate))
}
fn open(endpoint: &mut Endpoint, key: u32, channels: u32, rate: u32, samples: &[i16]) {
    endpoint
        .open_sound(key, channels, rate, samples.len() as u32 / channels, &pcm(samples))
        .unwrap();
}
fn play(endpoint: &mut Endpoint, key: u32, instance: u32, frames: u64, flags: u32) {
    let rate = endpoint.sound(key).unwrap().sample_rate;
    endpoint.play(key, instance, abi::UNITY_GAIN, 0, timestamp(frames, rate), flags).unwrap();
}

#[test]
fn reused_sound_key_keeps_queued_close_before_new_playback() {
    // A guest capability slot may be reused while old commands retain PCM.
    // Closing the old sound precedes every command for the replacement in FIFO.
    let _serial = TEST_SERIAL.lock().unwrap();
    let mut endpoint = endpoint(8_000);
    open(&mut endpoint, 7, 1, 8_000, &[100; 2]);
    play(&mut endpoint, 7, 1234, 2, abi::SOUND_LOOP);
    endpoint.set_gain(7, 1234, 0).unwrap();
    endpoint.close_sound(7).unwrap();
    open(&mut endpoint, 7, 1, 8_000, &[900; 2]);
    play(&mut endpoint, 7, 1234, 2, abi::SOUND_LOOP);
    let mut mixer = mixer::Mixer::new();
    let mut output = [0; 4];
    assert!(mixer.render(&endpoint.shared, &mut output));
    assert_eq!(output, [900; 4]);
    endpoint.close_sound(7).unwrap();
    assert!(!mixer.render(&endpoint.shared, &mut output));
    assert_eq!(output, [0; 4]);
}

#[test]
fn arbitrary_instance_ids_overlap_and_controls_are_scoped_to_the_sound() {
    // Render overlapping pairs to verify arbitrary IDs and independent control state.
    let _serial = TEST_SERIAL.lock().unwrap();
    let mut endpoint = endpoint(8_000);
    open(&mut endpoint, 10, 2, 8_000, &[1_000, 2_000, 3_000, 4_000]);
    open(&mut endpoint, 11, 1, 8_000, &[500, 500]);
    play(&mut endpoint, 10, 1, 2, abi::SOUND_LOOP);
    play(&mut endpoint, 10, 1234, 2, abi::SOUND_LOOP);
    play(&mut endpoint, 11, 1, 2, abi::SOUND_LOOP);
    endpoint.set_balance(10, 1, -1.0).unwrap();
    endpoint.set_gain(10, 1234, abi::UNITY_GAIN / 2).unwrap();
    let mut mixer = mixer::Mixer::new();
    let mut output = [0; 4];
    assert!(mixer.render(&endpoint.shared, &mut output));
    assert_eq!(output, [2_000, 1_500, 5_000, 2_500]);
    endpoint.stop(10, 1).unwrap();
    assert_eq!(endpoint.tell(10, 1), Err(Error::InvalidArgument));
    assert!(mixer.render(&endpoint.shared, &mut output));
    assert_eq!(output, [1_000, 1_500, 2_000, 2_500]);
    // Replacing the same pair restarts at its segment start and centers balance.
    play(&mut endpoint, 10, 1234, 2, 0);
    endpoint.stop(11, 1).unwrap();
    assert!(mixer.render(&endpoint.shared, &mut output));
    assert_eq!(output, [1_000, 2_000, 3_000, 4_000]);
    assert_eq!(endpoint.tell(10, 1234).unwrap(), timestamp(2, 8_000));
    assert!(!mixer.render(&endpoint.shared, &mut output));
}

#[test]
fn linear_resampling_preserves_channels_and_mono_is_duplicated() {
    // Compare exact resampled frames for stereo upsampling and mono downsampling.
    let _serial = TEST_SERIAL.lock().unwrap();
    let mut endpoint = endpoint(16_000);
    open(&mut endpoint, 1, 2, 8_000, &[0, 2_000, 2_000, 6_000]);
    play(&mut endpoint, 1, u32::MAX, 2, 0);
    let mut mixer = mixer::Mixer::new();
    let mut output = [0; 8];
    assert!(mixer.render(&endpoint.shared, &mut output));
    assert_eq!(output, [0, 2_000, 1_000, 4_000, 2_000, 6_000, 2_000, 6_000]);
    assert_eq!(endpoint.tell(1, u32::MAX).unwrap(), timestamp(2, 8_000));
    open(&mut endpoint, 2, 1, 32_000, &[100, 200, 300, 400]);
    play(&mut endpoint, 2, 3, 4, 0);
    mixer.render(&endpoint.shared, &mut output);
    assert_eq!(output, [100, 100, 300, 300, 0, 0, 0, 0]);
}

#[test]
fn resampling_44100_to_48000_keeps_an_exact_source_cursor() {
    // One second of output must advance exactly one second of source frames,
    // without accumulated floating point phase error at this common ratio.
    let _serial = TEST_SERIAL.lock().unwrap();
    let mut endpoint = endpoint(48_000);
    let samples = vec![700; 44_101];
    open(&mut endpoint, 1, 1, 44_100, &samples);
    play(&mut endpoint, 1, 0, samples.len() as u64, 0);
    let mut mixer = mixer::Mixer::new();
    let mut output = vec![0; 48_000 * 2];
    mixer.render(&endpoint.shared, &mut output);
    assert!(output.iter().all(|sample| *sample == 700));
    assert_eq!(endpoint.tell(1, 0).unwrap(), NANOSECONDS);
}

#[test]
fn segment_bounds_round_down_and_seek_tell_roundtrip_at_44100_hz() {
    // Exercise frame rounding and segment boundaries through real render and seek calls.
    let _serial = TEST_SERIAL.lock().unwrap();
    let rate = 44_100;
    let mut endpoint = endpoint(rate);
    open(&mut endpoint, 1, 1, rate, &[100, 200, 300, 400, 500]);
    endpoint.play(1, 7, abi::UNITY_GAIN, timestamp(1, rate) + 1, timestamp(3, rate), 0).unwrap();
    let mut mixer = mixer::Mixer::new();
    let mut output = [0; 2];
    mixer.render(&endpoint.shared, &mut output);
    assert_eq!(output, [200; 2]);
    let position = endpoint.tell(1, 7).unwrap();
    assert_eq!(position, timestamp(2, rate));
    endpoint.seek(1, 7, position).unwrap();
    assert_eq!(endpoint.tell(1, 7).unwrap(), position);
    mixer.render(&endpoint.shared, &mut output);
    assert_eq!(output, [300; 2]);
    endpoint.seek(1, 7, timestamp(1, rate)).unwrap();
    // The admitted seek is visible before the callback applies it.
    assert_eq!(endpoint.tell(1, 7).unwrap(), timestamp(1, rate));
    mixer.render(&endpoint.shared, &mut output);
    assert_eq!(output, [200; 2]);
    endpoint.seek(1, 7, timestamp(4, rate)).unwrap();
    assert!(!mixer.render(&endpoint.shared, &mut output));
    assert_eq!(output, [0; 2]);
    assert_eq!(endpoint.tell(1, 7).unwrap(), timestamp(4, rate));
    assert_eq!(endpoint.seek(1, 7, 0), Err(Error::InvalidArgument));
    assert_eq!(endpoint.seek(1, 7, timestamp(5, rate)), Err(Error::InvalidArgument));
    assert_eq!(endpoint.play(1, 8, 1, 0, 0, 0), Err(Error::InvalidArgument));
    assert_eq!(endpoint.play(1, 8, 1, 0, 1, 0), Err(Error::InvalidArgument));
    assert_eq!(endpoint.play(1, 8, 1, 0, timestamp(6, rate), 0), Err(Error::InvalidArgument));
}

#[test]
fn loop_segment_master_volume_and_balance_have_no_channel_amplification() {
    // Render a selected loop with gain stages and reject invalid control values.
    let _serial = TEST_SERIAL.lock().unwrap();
    let mut endpoint = endpoint(8_000);
    open(&mut endpoint, 1, 2, 8_000, &[1_000, 2_000, 3_000, 4_000, 5_000, 6_000]);
    endpoint
        .play(1, 0, abi::UNITY_GAIN, timestamp(1, 8_000), timestamp(2, 8_000), abi::SOUND_LOOP)
        .unwrap();
    endpoint.set_volume(0.5).unwrap();
    endpoint.set_balance(1, 0, 0.5).unwrap();
    let mut mixer = mixer::Mixer::new();
    let mut output = [0; 6];
    mixer.render(&endpoint.shared, &mut output);
    assert_eq!(output, [750, 2_000, 1_250, 3_000, 750, 2_000]);
    assert_eq!(endpoint.volume(), 0.5);
    assert_eq!(endpoint.set_volume(f32::NAN), Err(Error::InvalidArgument));
    assert_eq!(endpoint.set_volume(1.1), Err(Error::InvalidArgument));
    endpoint.set_volume(-0.0).unwrap();
    assert_eq!(endpoint.volume().to_bits(), 0);
    assert_eq!(endpoint.set_balance(1, 0, f32::INFINITY), Err(Error::InvalidArgument));
    assert_eq!(endpoint.set_balance(1, 0, -1.1), Err(Error::InvalidArgument));
    assert_eq!(endpoint.set_gain(1, 0, abi::UNITY_GAIN + 1), Err(Error::InvalidArgument));
    assert_eq!(endpoint.play(1, 0, 1, 0, timestamp(1, 8_000), 2), Err(Error::InvalidArgument));
}

#[test]
fn completed_instances_keep_their_position_until_stop_releases_capacity() {
    // Fill the pair bound, finish playback, then stop and reuse the released capacity.
    let _serial = TEST_SERIAL.lock().unwrap();
    let mut endpoint = endpoint(8_000);
    open(&mut endpoint, 1, 1, 8_000, &[i16::MAX]);
    for id in 0..abi::MAX_PLAYBACK_INSTANCES {
        play(&mut endpoint, 1, id as u32 + 100, 1, 0);
    }
    let mut mixer = mixer::Mixer::new();
    let mut output = [0; 2];
    mixer.render(&endpoint.shared, &mut output);
    assert_eq!(output, [i16::MAX; 2]);
    assert_eq!(endpoint.tell(1, 100).unwrap(), timestamp(1, 8_000));
    assert_eq!(endpoint.play(1, 1234, 1, 0, timestamp(1, 8_000), 0), Err(Error::LimitExceeded));
    endpoint.stop(1, 100).unwrap();
    play(&mut endpoint, 1, 1234, 1, 0);
    mixer.render(&endpoint.shared, &mut output);
    assert_eq!(output, [i16::MAX; 2]);
    endpoint.seek(1, 1234, 0).unwrap();
    mixer.render(&endpoint.shared, &mut output);
    assert_eq!(output, [i16::MAX; 2]);
}

#[test]
fn queue_rejection_does_not_replace_an_instance_or_publish_a_seek() {
    // Fill the command queue and verify rejected calls preserve admitted playback.
    let _serial = TEST_SERIAL.lock().unwrap();
    let mut endpoint = endpoint(8_000);
    open(&mut endpoint, 1, 1, 8_000, &[100, 200]);
    play(&mut endpoint, 1, 0, 2, abi::SOUND_LOOP);
    let mut mixer = mixer::Mixer::new();
    mixer.render(&endpoint.shared, &mut [0; 2]);
    for _ in 0..abi::MAX_COMMANDS {
        endpoint.set_gain(1, 0, abi::UNITY_GAIN).unwrap();
    }
    assert_eq!(endpoint.seek(1, 0, 0), Err(Error::LimitExceeded));
    assert_eq!(endpoint.tell(1, 0).unwrap(), timestamp(1, 8_000));
    assert_eq!(endpoint.play(1, 0, 1, 0, timestamp(2, 8_000), 0), Err(Error::LimitExceeded));
    assert_eq!(endpoint.play_references, 1);
    let mut output = [0; 2];
    mixer.render(&endpoint.shared, &mut output);
    assert_eq!(output, [200; 2]);
}

#[test]
fn closed_pcm_retains_aggregate_credit_until_the_callback_releases_it() {
    // Exhaust PCM memory and release it only after callback retirement is reclaimed.
    let _serial = TEST_SERIAL.lock().unwrap();
    let mut endpoint = endpoint(8_000);
    assert_eq!(endpoint.open_sound(1, 1, 8_000, 1, &[0]), Err(Error::InvalidArgument));
    let bytes = vec![0; abi::MAX_SOUND_BYTES];
    let frames = bytes.len() as u32 / 2;
    endpoint.open_sound(1, 1, 8_000, frames, &bytes).unwrap();
    play(&mut endpoint, 1, 0, 1, abi::SOUND_LOOP);
    endpoint.close_sound(1).unwrap();
    assert_eq!(endpoint.bytes, abi::MAX_SOUND_BYTES);
    assert_eq!(endpoint.open_sound(2, 1, 8_000, frames, &bytes), Err(Error::LimitExceeded));
    let mut mixer = mixer::Mixer::new();
    assert!(!mixer.render(&endpoint.shared, &mut [0; 2]));
    endpoint.reclaim();
    assert_eq!(endpoint.bytes, 0);
    endpoint.open_sound(2, 1, 8_000, frames, &bytes).unwrap();
}

#[test]
fn callback_retirement_is_bounded_even_when_the_producer_is_preempted() {
    // Retire all queued replacements and mixer references before producer reclamation.
    let _serial = TEST_SERIAL.lock().unwrap();
    let mut endpoint = endpoint(8_000);
    open(&mut endpoint, 1, 1, 8_000, &[100]);
    let mut mixer = mixer::Mixer::new();
    for id in 0..abi::MAX_PLAYBACK_INSTANCES {
        play(&mut endpoint, 1, id as u32, 1, abi::SOUND_LOOP);
    }
    mixer.render(&endpoint.shared, &mut []);
    for _ in 0..abi::MAX_COMMANDS {
        play(&mut endpoint, 1, 0, 1, abi::SOUND_LOOP);
    }
    endpoint.reclaim();
    assert_eq!(endpoint.play_references, RECLAIM_CAPACITY);
    mixer.render(&endpoint.shared, &mut []);
    endpoint.halt();
    mixer.render(&endpoint.shared, &mut []);
    assert_eq!(endpoint.shared.reclaim.len(), RECLAIM_CAPACITY);
    assert_eq!(endpoint.reserve_play_reference(), Err(Error::LimitExceeded));
    endpoint.reclaim();
    assert_eq!(endpoint.play_references, 0);
}

#[test]
fn visibility_preserves_loops_and_discards_one_shots_and_hidden_play() {
    // Change presentation epochs and render to distinguish loop and one-shot behavior.
    let _serial = TEST_SERIAL.lock().unwrap();
    let mut endpoint = endpoint(8_000);
    open(&mut endpoint, 1, 1, 8_000, &[100, 200]);
    play(&mut endpoint, 1, 0, 2, abi::SOUND_LOOP);
    play(&mut endpoint, 1, 1, 2, 0);
    let mut mixer = mixer::Mixer::new();
    let mut output = [0; 2];
    mixer.render(&endpoint.shared, &mut output);
    assert_eq!(output, [200; 2]);
    endpoint.shared.presentation.activate(false);
    assert_eq!(endpoint.play(1, 2, 1, 0, timestamp(1, 8_000), 0), Err(Error::Unavailable));
    assert!(!mixer.render(&endpoint.shared, &mut output));
    assert_eq!(output, [0; 2]);
    endpoint.shared.presentation.activate(true);
    assert!(mixer.render(&endpoint.shared, &mut output));
    assert_eq!(output, [200; 2]);
    endpoint.shared.presentation.activate(false);
    endpoint.stop(1, 0).unwrap();
    mixer.render(&endpoint.shared, &mut output);
    endpoint.shared.presentation.activate(true);
    assert!(!mixer.render(&endpoint.shared, &mut output));
}

#[test]
fn presentation_snapshots_pair_readiness_with_the_matching_epoch() {
    // Visibility and foreground transitions publish flags and epoch atomically;
    // a presentation registered while backgrounded inherits that state.
    let _serial = TEST_SERIAL.lock().unwrap();
    set_all_active(false);
    let presentation = Presentation::new([97; 16]);
    let initial = presentation.snapshot();
    assert!(!initial.0);
    presentation.activate(true);
    let hidden = presentation.snapshot();
    assert!(!hidden.0);
    assert!(hidden.1 > initial.1);
    set_all_active(true);
    let active = presentation.snapshot();
    assert!(active.0);
    assert!(active.1 > hidden.1);
    presentation.activate(true);
    set_all_active(true);
    assert_eq!(presentation.snapshot(), active);
    presentation.activate(false);
    let hidden = presentation.snapshot();
    assert!(!hidden.0);
    assert!(hidden.1 > active.1);
}

#[test]
fn a_resumed_play_is_not_discarded_with_the_preceding_hidden_epoch() {
    // Interleave lifecycle changes between callback command stages. A command
    // admitted after resume must survive even if earlier callback work was hidden.
    let _serial = TEST_SERIAL.lock().unwrap();
    let mut endpoint = endpoint(8_000);
    open(&mut endpoint, 1, 1, 8_000, &[1_000]);
    let mut mixer = mixer::Mixer::new();
    play(&mut endpoint, 1, 0, 1, 0);
    mixer.command(endpoint.shared.commands.pop().unwrap(), &endpoint.shared);
    endpoint.shared.presentation.activate(false);
    assert!(!endpoint.shared.is_active());
    endpoint.shared.presentation.activate(true);
    play(&mut endpoint, 1, 1234, 1, 0);
    mixer.command(endpoint.shared.commands.pop().unwrap(), &endpoint.shared);
    let mut output = [0; 2];
    assert!(mixer.render(&endpoint.shared, &mut output));
    assert_eq!(output, [1_000; 2]);
    assert_eq!(endpoint.tell(1, 0).unwrap(), 0);
    assert_eq!(endpoint.tell(1, 1234).unwrap(), timestamp(1, 8_000));
}

#[cfg(target_vendor = "apple")]
#[test]
fn parked_worker_publishes_resumed_playback_until_completion_or_stop() {
    // Resume before the parked worker drains commands. Its zero-frame render
    // must expose new one-shot work to the replacement device while dropping
    // old-epoch one-shots; paused loops keep their exact position too.
    let _serial = TEST_SERIAL.lock().unwrap();
    let mut endpoint = endpoint(8_000);
    open(&mut endpoint, 1, 1, 8_000, &[1_000, 2_000]);
    let mut mixer = mixer::Mixer::new();
    play(&mut endpoint, 1, 0, 2, 0);
    let epoch = endpoint.shared.presentation.epoch();
    endpoint.shared.presentation.activate(false);
    endpoint.shared.presentation.activate(true);
    play(&mut endpoint, 1, 1, 2, 0);
    assert!(apple::wait_for_resume(&endpoint.shared, epoch, &mut mixer));
    assert!(endpoint.shared.commands.is_empty());
    assert!(endpoint.shared.playing.load(Ordering::Acquire), "resumed one-shot needs output");
    assert_eq!(endpoint.tell(1, 0).unwrap(), 0);
    assert_eq!(endpoint.tell(1, 1).unwrap(), 0);

    let mut output = [0; 2];
    assert!(mixer.render(&endpoint.shared, &mut output));
    assert_eq!(output, [1_000; 2]);
    assert_eq!(endpoint.tell(1, 0).unwrap(), 0);
    assert_eq!(endpoint.tell(1, 1).unwrap(), timestamp(1, 8_000));
    assert!(mixer.render(&endpoint.shared, &mut output));
    assert_eq!(output, [2_000; 2]);
    assert_eq!(endpoint.tell(1, 1).unwrap(), timestamp(2, 8_000));
    assert!(!endpoint.shared.playing.load(Ordering::Acquire));
    assert!(!mixer.render(&endpoint.shared, &mut output));
    assert_eq!(output, [0; 2]);

    play(&mut endpoint, 1, 2, 2, abi::SOUND_LOOP);
    assert!(mixer.render(&endpoint.shared, &mut output));
    assert_eq!(output, [1_000; 2]);
    assert!(endpoint.shared.playing.load(Ordering::Acquire));
    endpoint.shared.presentation.activate(false);
    let epoch = endpoint.shared.presentation.epoch();
    assert!(!mixer.render(&endpoint.shared, &mut []));
    assert!(endpoint.shared.playing.load(Ordering::Acquire), "paused loop retains readiness");
    endpoint.shared.presentation.activate(true);
    assert!(apple::wait_for_resume(&endpoint.shared, epoch, &mut mixer));
    assert!(endpoint.shared.playing.load(Ordering::Acquire));
    assert_eq!(endpoint.tell(1, 2).unwrap(), timestamp(1, 8_000));
    assert!(mixer.render(&endpoint.shared, &mut output));
    assert_eq!(output, [2_000; 2]);
    endpoint.stop(1, 2).unwrap();
    assert!(!mixer.render(&endpoint.shared, &mut []));
    assert!(!endpoint.shared.playing.load(Ordering::Acquire));
}

#[cfg(target_vendor = "apple")]
#[test]
fn inactive_worker_drains_cleanup_without_resuming_output() {
    // Wake a parked device owner with cleanup and verify PCM can be reclaimed.
    let _serial = TEST_SERIAL.lock().unwrap();
    let mut endpoint = endpoint(8_000);
    open(&mut endpoint, 1, 1, 8_000, &[100]);
    play(&mut endpoint, 1, 0, 1, abi::SOUND_LOOP);
    let mut mixer = mixer::Mixer::new();
    mixer.render(&endpoint.shared, &mut [0; 2]);
    endpoint.shared.presentation.activate(false);
    endpoint.close_sound(1).unwrap();
    let shared = endpoint.shared.clone();
    let epoch = shared.presentation.epoch();
    let worker = std::thread::spawn(move || apple::wait_for_resume(&shared, epoch, &mut mixer));
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while !endpoint.shared.commands.is_empty() && std::time::Instant::now() < deadline {
        std::thread::yield_now();
    }
    endpoint.shared.quit.store(true, Ordering::Release);
    endpoint.shared.notify();
    assert!(!worker.join().unwrap());
    endpoint.reclaim();
    assert_eq!(endpoint.bytes, 0);
}
