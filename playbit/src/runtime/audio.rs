//! Per-instance PCM ownership and nonblocking playback admission. Resource
//! capabilities belong to State; this endpoint owns only admitted sound keys.
//! The callback mixes fixed storage; allocation and reclamation stay off it.
use crossbeam_queue::ArrayQueue;
use playbit_api::abi::{audio as abi, syscall::Error};
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
    },
};

#[cfg(target_vendor = "apple")]
mod apple;
mod mixer;
#[cfg(test)]
mod tests;
mod wake;

const MAX_ENDPOINTS: usize = 8; // Four visible apps and their retiring outputs.
const RECLAIM_CAPACITY: usize = abi::MAX_COMMANDS + abi::MAX_PLAYBACK_INSTANCES;
const NANOSECONDS: u64 = 1_000_000_000;
const VISIBLE: u64 = 1;
const IN_FOREGROUND: u64 = 2;
const LIFECYCLE_FLAGS: u64 = VISIBLE | IN_FOREGROUND;
const EPOCH_INCREMENT: u64 = LIFECYCLE_FLAGS + 1;
static REGISTRY: Mutex<Vec<Weak<Shared>>> = Mutex::new(Vec::new());
static PRESENTATIONS: Mutex<Vec<Weak<Presentation>>> = Mutex::new(Vec::new());
static FOREGROUND: AtomicBool = AtomicBool::new(true);
#[cfg(test)]
pub(crate) static TEST_SERIAL: Mutex<()> = Mutex::new(());

struct Sound {
    key: u32,
    channels: u32,
    sample_rate: u32,
    frames: u32,
    samples: Box<[i16]>,
}
struct Playback {
    sound: Arc<Sound>,
    instance: u32,
    start: u32,
    end: u32,
    position: AtomicU64, // Applied seek sequence in high32, source frame in low32.
    seek: AtomicU64,     // Last admitted seek sequence and requested source frame.
}
enum Command {
    Play { slot: usize, playback: Arc<Playback>, looping: bool, gain: u32, epoch: u64 },
    Stop(usize),
    Gain { slot: usize, gain: u32 },
    Balance { slot: usize, left: u32, right: u32 },
    Seek { slot: usize, frame: u32, sequence: u32 },
    Close(u32),
}
/// Lifecycle ownership precedes audio output, so hidden guests cannot escape an
/// earlier visibility notification by opening their audio capability later.
pub(crate) struct Presentation {
    instance: [u8; 16],
    lifecycle: AtomicU64,
    wake: Mutex<Option<Weak<Shared>>>,
}
impl Presentation {
    pub(crate) fn new(instance: [u8; 16]) -> Arc<Self> {
        let mut presentations = PRESENTATIONS.lock().expect("audio presentations poisoned");
        presentations.retain(|entry| entry.strong_count() != 0);
        let foreground = if FOREGROUND.load(Ordering::Acquire) { IN_FOREGROUND } else { 0 };
        let presentation = Arc::new(Self {
            instance,
            lifecycle: AtomicU64::new(EPOCH_INCREMENT | foreground),
            wake: Mutex::new(None),
        });
        presentations.push(Arc::downgrade(&presentation));
        presentation
    }
    fn notify(&self) {
        if let Some(shared) = self
            .wake
            .lock()
            .expect("audio visibility wake poisoned")
            .as_ref()
            .and_then(Weak::upgrade)
        {
            shared.notify();
        }
    }
    fn activate(&self, active: bool) {
        self.update_lifecycle(VISIBLE, active);
    }
    fn update_lifecycle(&self, flag: u64, enabled: bool) {
        let changed = self.lifecycle.try_update(Ordering::AcqRel, Ordering::Acquire, |state| {
            if (state & flag != 0) == enabled {
                return None;
            }
            let state =
                state.checked_add(EPOCH_INCREMENT).expect("audio lifecycle epoch exhausted");
            Some(if enabled { state | flag } else { state & !flag })
        });
        if changed.is_ok() {
            self.notify();
        }
    }
    fn snapshot(&self) -> (bool, u64) {
        let state = self.lifecycle.load(Ordering::Acquire);
        (state & LIFECYCLE_FLAGS == LIFECYCLE_FLAGS, state / EPOCH_INCREMENT)
    }
    fn epoch(&self) -> u64 {
        self.snapshot().1
    }
}
struct Shared {
    presentation: Arc<Presentation>,
    sample_rate: u32,
    volume: AtomicU32,
    commands: ArrayQueue<Command>,
    reclaim: ArrayQueue<Arc<Playback>>,
    wake: wake::Wake,
    quit: AtomicBool,
    failed: AtomicBool,
    playing: AtomicBool, // Retained mixer work; paused loops remain ready for output.
    halted: AtomicBool,
}
impl Shared {
    fn is_active(&self) -> bool {
        self.lifecycle().0
    }
    fn lifecycle(&self) -> (bool, u64) {
        let (active, epoch) = self.presentation.snapshot();
        (active && !self.halted.load(Ordering::Acquire), epoch)
    }
    fn notify(&self) {
        if !self.wake.notify() {
            self.failed.store(true, Ordering::Release);
        }
    }
    fn wait(&self) -> bool {
        if self.wake.wait() {
            return true;
        }
        self.failed.store(true, Ordering::Release);
        false
    }
    fn retire(&self, playback: Arc<Playback>) {
        // Every accepted Play holds a reference credit across the command,
        // mixer and retirement queues. Only producer reclamation releases it.
        assert!(self.reclaim.push(playback).is_ok(), "bounded audio reclamation credit");
    }
}

/// Native presentation calls this on visibility changes, without guest code.
pub fn set_instance_active(instance: [u8; 16], active: bool) {
    let presentations = PRESENTATIONS.lock().expect("audio presentations poisoned");
    for presentation in presentations.iter().filter_map(Weak::upgrade) {
        if presentation.instance == instance {
            presentation.activate(active);
        }
    }
}
/// App backgrounding silences every instance without changing scene state.
pub fn set_all_active(active: bool) {
    let presentations = PRESENTATIONS.lock().expect("audio presentations poisoned");
    FOREGROUND.store(active, Ordering::Release);
    for presentation in presentations.iter().filter_map(Weak::upgrade) {
        presentation.update_lifecycle(IN_FOREGROUND, active);
    }
}

fn timestamp_frame(timestamp: u64, sample_rate: u32) -> Result<u32, Error> {
    let frame = u128::from(timestamp) * u128::from(sample_rate) / u128::from(NANOSECONDS);
    u32::try_from(frame).map_err(|_| Error::InvalidArgument)
}
fn frame_timestamp(frame: u64, sample_rate: u32) -> u64 {
    (frame * NANOSECONDS).div_ceil(u64::from(sample_rate))
}

pub(crate) struct Endpoint {
    sounds: BTreeMap<u32, Arc<Sound>>,
    retired: Vec<Arc<Sound>>,
    playbacks: [Option<Arc<Playback>>; abi::MAX_PLAYBACK_INSTANCES],
    bytes: usize,
    play_references: usize,
    shared: Arc<Shared>,
    driver: Option<std::thread::JoinHandle<()>>,
}
impl Endpoint {
    pub(crate) fn new(presentation: Arc<Presentation>, preferred_rate: u32) -> Result<Self, Error> {
        let sample_rate =
            if preferred_rate == 0 { abi::DEFAULT_SAMPLE_RATE } else { preferred_rate };
        if !(abi::MIN_SAMPLE_RATE..=abi::MAX_SAMPLE_RATE).contains(&sample_rate) {
            return Err(Error::InvalidArgument);
        }
        let mut registry = REGISTRY.lock().expect("audio registry poisoned");
        registry.retain(|entry| entry.strong_count() != 0);
        if registry.len() == MAX_ENDPOINTS {
            return Err(Error::LimitExceeded);
        }
        let shared = Arc::new(Shared {
            presentation: presentation.clone(),
            sample_rate,
            volume: AtomicU32::new(1.0_f32.to_bits()),
            commands: ArrayQueue::new(abi::MAX_COMMANDS),
            reclaim: ArrayQueue::new(RECLAIM_CAPACITY),
            wake: wake::Wake::new()?,
            quit: AtomicBool::new(false),
            failed: AtomicBool::new(false),
            playing: AtomicBool::new(false),
            halted: AtomicBool::new(false),
        });
        *presentation.wake.lock().expect("audio visibility wake poisoned") =
            Some(Arc::downgrade(&shared));
        registry.push(Arc::downgrade(&shared));
        Ok(Self {
            sounds: BTreeMap::new(),
            retired: Vec::with_capacity(abi::MAX_SOUNDS),
            playbacks: std::array::from_fn(|_| None),
            bytes: 0,
            play_references: 0,
            shared,
            driver: None,
        })
    }
    /// Called only by the asynchronous open worker, never by a guest turn.
    pub(crate) fn initialize(&mut self) -> Result<(), Error> {
        if self.driver.is_some() {
            return Err(Error::InvalidArgument);
        }
        #[cfg(target_vendor = "apple")]
        {
            let shared = self.shared.clone();
            let (ready, initialized) = std::sync::mpsc::sync_channel(1);
            self.driver = Some(
                std::thread::Builder::new()
                    .name("playbit-audio".into())
                    .spawn(move || apple::run(shared, ready))
                    .map_err(|_| Error::Unavailable)?,
            );
            initialized.recv().map_err(|_| Error::Unavailable)?
        }
        #[cfg(not(target_vendor = "apple"))]
        {
            Err(Error::Unsupported)
        }
    }
    pub(crate) fn sample_rate(&self) -> u32 {
        self.shared.sample_rate
    }
    pub(crate) fn volume(&self) -> f32 {
        f32::from_bits(self.shared.volume.load(Ordering::Acquire))
    }
    pub(crate) fn set_volume(&self, volume: f32) -> Result<(), Error> {
        if !volume.is_finite() || !(0.0..=1.0).contains(&volume) {
            return Err(Error::InvalidArgument);
        }
        let volume = if volume == 0.0 { 0.0 } else { volume };
        self.shared.volume.store(volume.to_bits(), Ordering::Release);
        Ok(())
    }
    fn reclaim(&mut self) {
        while self.shared.reclaim.pop().is_some() {
            self.play_references =
                self.play_references.checked_sub(1).expect("retired Play owns a reference credit");
        }
        self.retired.retain(|sound| {
            if Arc::strong_count(sound) != 1 {
                return true;
            }
            self.bytes -= sound.samples.len() * 2;
            false
        });
        assert!(self.bytes <= abi::MAX_SOUND_BYTES);
        assert!(self.sounds.len() + self.retired.len() <= abi::MAX_SOUNDS);
    }
    pub(crate) fn open_sound(
        &mut self,
        key: u32,
        channels: u32,
        rate: u32,
        frames: u32,
        bytes: &[u8],
    ) -> Result<(), Error> {
        if abi::pcm_byte_count(channels, rate, frames)? != bytes.len() {
            return Err(Error::InvalidArgument);
        }
        self.reclaim();
        if self.sounds.contains_key(&key) {
            return Err(Error::InvalidArgument);
        }
        if bytes.len() > abi::MAX_SOUND_BYTES - self.bytes
            || self.sounds.len() + self.retired.len() == abi::MAX_SOUNDS
        {
            return Err(Error::LimitExceeded);
        }
        let samples = bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|b| i16::from_le_bytes([b[0], b[1]]))
            .collect::<Vec<_>>()
            .into_boxed_slice();
        self.sounds
            .insert(key, Arc::new(Sound { key, channels, sample_rate: rate, frames, samples }));
        self.bytes += bytes.len();
        assert!(self.bytes <= abi::MAX_SOUND_BYTES);
        Ok(())
    }
    fn sound(&self, key: u32) -> Result<&Arc<Sound>, Error> {
        self.sounds.get(&key).ok_or(Error::InvalidArgument)
    }
    fn slot(&self, key: u32, instance: u32) -> Result<usize, Error> {
        self.playbacks
            .iter()
            .position(|playback| {
                playback.as_ref().is_some_and(|p| p.sound.key == key && p.instance == instance)
            })
            .ok_or(Error::InvalidArgument)
    }
    fn send(&mut self, command: Command) -> Result<(), Error> {
        self.reclaim();
        let play = matches!(&command, Command::Play { .. });
        if play {
            self.reserve_play_reference()?;
        }
        if self.shared.commands.push(command).is_err() {
            if play {
                self.play_references -= 1;
            }
            return Err(Error::LimitExceeded);
        }
        self.shared.notify();
        Ok(())
    }
    fn reserve_play_reference(&mut self) -> Result<(), Error> {
        assert!(self.play_references <= RECLAIM_CAPACITY);
        if self.play_references == RECLAIM_CAPACITY {
            return Err(Error::LimitExceeded);
        }
        self.play_references += 1;
        Ok(())
    }
    pub(crate) fn close_sound(&mut self, key: u32) -> Result<(), Error> {
        self.sound(key)?;
        self.send(Command::Close(key))?;
        for playback in &mut self.playbacks {
            if playback.as_ref().is_some_and(|playback| playback.sound.key == key) {
                *playback = None;
            }
        }
        self.retired.push(self.sounds.remove(&key).expect("admitted sound remains owned"));
        self.reclaim();
        Ok(())
    }
    #[allow(clippy::too_many_arguments, reason = "Playback admission mirrors the scalar API.")]
    pub(crate) fn play(
        &mut self,
        key: u32,
        instance: u32,
        gain: u32,
        start: u64,
        length: u64,
        flags: u32,
    ) -> Result<(), Error> {
        if gain > abi::UNITY_GAIN || flags & !abi::SOUND_LOOP != 0 || length == 0 {
            return Err(Error::InvalidArgument);
        }
        let sound = self.sound(key)?;
        let start = timestamp_frame(start, sound.sample_rate)?;
        let length = timestamp_frame(length, sound.sample_rate)?;
        let end = start.checked_add(length).ok_or(Error::InvalidArgument)?;
        if length == 0 || end > sound.frames {
            return Err(Error::InvalidArgument);
        }
        let (active, epoch) = self.shared.lifecycle();
        if self.shared.failed.load(Ordering::Acquire) || !active {
            return Err(Error::Unavailable);
        }
        let slot = self
            .slot(key, instance)
            .ok()
            .or_else(|| self.playbacks.iter().position(Option::is_none))
            .ok_or(Error::LimitExceeded)?;
        let playback = Arc::new(Playback {
            sound: sound.clone(),
            instance,
            start,
            end,
            position: AtomicU64::new(u64::from(start)),
            seek: AtomicU64::new(0),
        });
        self.send(Command::Play {
            slot,
            playback: playback.clone(),
            looping: flags & abi::SOUND_LOOP != 0,
            gain,
            epoch,
        })?;
        self.playbacks[slot] = Some(playback);
        Ok(())
    }
    pub(crate) fn tell(&self, key: u32, instance: u32) -> Result<u64, Error> {
        let playback = self.playbacks[self.slot(key, instance)?]
            .as_ref()
            .expect("admitted playback slot remains owned");
        let seek = playback.seek.load(Ordering::Acquire);
        let rendered = playback.position.load(Ordering::Acquire);
        let cursor = if seek >> 32 > rendered >> 32 { seek } else { rendered };
        Ok(frame_timestamp(u64::from(cursor as u32), playback.sound.sample_rate))
    }
    pub(crate) fn seek(&mut self, key: u32, instance: u32, position: u64) -> Result<(), Error> {
        let slot = self.slot(key, instance)?;
        let playback =
            self.playbacks[slot].as_ref().expect("admitted playback slot remains owned").clone();
        let frame = timestamp_frame(position, playback.sound.sample_rate)?;
        if frame < playback.start || frame > playback.end {
            return Err(Error::InvalidArgument);
        }
        let previous = (playback.seek.load(Ordering::Acquire) >> 32) as u32;
        let sequence = previous.checked_add(1).ok_or(Error::LimitExceeded)?;
        self.send(Command::Seek { slot, frame, sequence })?;
        playback.seek.store((u64::from(sequence) << 32) | u64::from(frame), Ordering::Release);
        Ok(())
    }
    pub(crate) fn set_gain(&mut self, key: u32, instance: u32, gain: u32) -> Result<(), Error> {
        if gain > abi::UNITY_GAIN {
            return Err(Error::InvalidArgument);
        }
        self.send(Command::Gain { slot: self.slot(key, instance)?, gain })
    }
    pub(crate) fn set_balance(
        &mut self,
        key: u32,
        instance: u32,
        balance: f32,
    ) -> Result<(), Error> {
        if !balance.is_finite() || !(-1.0..=1.0).contains(&balance) {
            return Err(Error::InvalidArgument);
        }
        let left = ((1.0 - balance.max(0.0)) * abi::UNITY_GAIN as f32) as u32;
        let right = ((1.0 + balance.min(0.0)) * abi::UNITY_GAIN as f32) as u32;
        self.send(Command::Balance { slot: self.slot(key, instance)?, left, right })
    }
    pub(crate) fn stop(&mut self, key: u32, instance: u32) -> Result<(), Error> {
        let slot = self.slot(key, instance)?;
        self.send(Command::Stop(slot))?;
        self.playbacks[slot] = None;
        Ok(())
    }
    pub(crate) fn halt(&self) {
        self.shared.halted.store(true, Ordering::Release);
        self.shared.presentation.activate(false);
        self.shared.notify();
    }
}
impl Drop for Endpoint {
    fn drop(&mut self) {
        self.shared.quit.store(true, Ordering::Release);
        self.shared.notify();
        // Detach rather than waiting on a guest turn. The device owner retains
        // Shared (and its registry credit) until synchronous queue disposal has
        // joined callbacks and reclaimed their storage on the device thread.
        self.driver.take();
    }
}
