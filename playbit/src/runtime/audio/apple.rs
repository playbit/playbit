//! AudioQueue output shared by macOS and iOS. The device thread creates and
//! disposes the queue. Callbacks only mix into three fixed 256-frame buffers.
//! Empty playback parks buffers and stops the queue; a command wake restarts
//! output without polling.
#![allow(unsafe_code, reason = "AudioToolbox C ABI and callback-owned PCM buffers.")]
use super::{Arc, ArrayQueue, Ordering, Shared, mixer::Mixer};
use playbit_api::abi::syscall::Error;
use std::{cell::UnsafeCell, ffi::c_void, ptr, sync::atomic::AtomicBool};
const BUFFER_COUNT: usize = 3;
const BUFFER_FRAMES: usize = 256;
type Queue = *mut c_void;
#[repr(C)]
struct Format {
    sample_rate: f64,
    format: u32,
    flags: u32,
    bytes_per_packet: u32,
    frames_per_packet: u32,
    bytes_per_frame: u32,
    channels: u32,
    bits: u32,
    reserved: u32,
}
#[repr(C)]
struct Buffer {
    capacity: u32,
    data: *mut c_void,
    size: u32,
    user_data: *mut c_void,
    packet_capacity: u32,
    packets: *mut c_void,
    packet_count: u32,
}
#[derive(Clone, Copy)]
struct FreeBuffer(*mut Buffer);
// SAFETY: ownership transfers through the free-buffer queue. Neither side
// accesses a buffer after enqueuing it to AudioQueue until its next callback.
unsafe impl Send for FreeBuffer {}
// SAFETY: FreeBuffer is only dereferenced by the queue's exclusive consumer.
unsafe impl Sync for FreeBuffer {}
struct Callback {
    shared: Arc<Shared>,
    free: ArrayQueue<FreeBuffer>,
    mixer: UnsafeCell<Mixer>,
    entered: AtomicBool,
}
// SAFETY: entered grants exclusive mixer access; remaining fields are atomic
// queues or immutable Arcs. AudioQueueDispose joins callbacks before destruction.
unsafe impl Sync for Callback {}
#[link(name = "AudioToolbox", kind = "framework")]
unsafe extern "C" {
    fn AudioQueueNewOutput(
        format: *const Format,
        callback: unsafe extern "C" fn(*mut c_void, Queue, *mut Buffer),
        user: *mut c_void,
        run_loop: *const c_void,
        mode: *const c_void,
        flags: u32,
        output: *mut Queue,
    ) -> i32;
    fn AudioQueueAllocateBuffer(queue: Queue, size: u32, output: *mut *mut Buffer) -> i32;
    fn AudioQueueEnqueueBuffer(
        queue: Queue,
        buffer: *mut Buffer,
        packets: u32,
        descriptions: *const c_void,
    ) -> i32;
    fn AudioQueueStart(queue: Queue, timestamp: *const c_void) -> i32;
    fn AudioQueueStop(queue: Queue, immediate: u8) -> i32;
    fn AudioQueueDispose(queue: Queue, immediate: u8) -> i32;
}

pub(super) fn run(shared: Arc<Shared>, ready: std::sync::mpsc::SyncSender<Result<(), Error>>) {
    let callback = Box::new(Callback {
        shared: shared.clone(),
        free: ArrayQueue::new(BUFFER_COUNT),
        mixer: UnsafeCell::new(Mixer::new()),
        entered: AtomicBool::new(false),
    });
    // Opening creates and validates output even for an initially hidden guest;
    // no samples are started until its presentation becomes active.
    let queue = match create(&callback) {
        Ok(queue) => queue,
        Err(status) => {
            device_failed(&shared, status);
            let _ = ready.send(Err(Error::Unavailable));
            return;
        }
    };
    let _ = ready.send(Ok(()));
    run_queues(queue, callback);
}
fn run_queues(mut queue: Queue, mut callback: Box<Callback>) {
    let shared = callback.shared.clone();
    loop {
        let epoch = shared.presentation.epoch();
        shared.notify();
        serve(queue, &callback, epoch);
        // SAFETY: synchronous disposal joins callbacks before their storage
        // can be reclaimed. A failure retains Shared and its registry credit.
        let status = unsafe { AudioQueueDispose(queue, 1) };
        if status != 0 {
            device_failed(&shared, status);
            Box::leak(callback);
            return;
        }
        while callback.free.pop().is_some() {}
        if !wait_for_resume(&shared, epoch, callback.mixer.get_mut()) {
            return;
        }
        queue = loop {
            let epoch = shared.presentation.epoch();
            match create(&callback) {
                Ok(queue) => break queue,
                Err(status) => {
                    device_failed(&shared, status);
                    if !wait_for_resume(&shared, epoch, callback.mixer.get_mut()) {
                        return;
                    }
                }
            }
        };
        shared.failed.store(false, Ordering::Release);
    }
}
fn device_failed(shared: &Shared, status: i32) {
    shared.failed.store(true, Ordering::Release);
    eprintln!("Native audio output unavailable (AudioQueue status {status})");
}
pub(super) fn wait_for_resume(shared: &Shared, epoch: u64, mixer: &mut Mixer) -> bool {
    while !shared.quit.load(Ordering::Acquire) {
        // No AudioQueue exists here. Drain cleanup on every wake without
        // advancing PCM, so hidden guests can close sounds and stop loops.
        mixer.render(shared, &mut []);
        if shared.is_active() && shared.presentation.epoch() != epoch {
            return true;
        }
        if !shared.wait() {
            return false;
        }
    }
    false
}
fn serve(queue: Queue, callback: &Callback, epoch: u64) {
    let shared = &callback.shared;
    let mut running = false;
    while !shared.quit.load(Ordering::Acquire) && shared.wait() {
        if shared.quit.load(Ordering::Acquire)
            || shared.failed.load(Ordering::Acquire)
            || !shared.is_active()
            || shared.presentation.epoch() != epoch
        {
            break;
        }
        let retained_playback = shared.playing.load(Ordering::Acquire);
        if shared.commands.is_empty() && !retained_playback {
            if running && callback.free.len() == BUFFER_COUNT {
                // SAFETY: all buffers have returned to the free queue, so no
                // callback can still mix or reenqueue one. The device owner
                // completes this stop before handling any later command wake.
                let status = unsafe { AudioQueueStop(queue, 1) };
                if status != 0 {
                    device_failed(shared, status);
                    break;
                }
                running = false;
            }
            continue;
        }
        match prime(queue, callback) {
            Ok(started) => running |= started,
            Err(status) => {
                device_failed(shared, status);
                break;
            }
        }
    }
}
fn create(callback: &Callback) -> Result<Queue, i32> {
    let format = Format {
        sample_rate: f64::from(callback.shared.sample_rate),
        format: u32::from_be_bytes(*b"lpcm"),
        flags: 4 | 8,
        bytes_per_packet: 4,
        frames_per_packet: 1,
        bytes_per_frame: 4,
        channels: 2,
        bits: 16,
        reserved: 0,
    };
    let mut queue = ptr::null_mut();
    // SAFETY: format is borrowed for creation; callback remains boxed until the
    // owning thread disposes the queue. PCM is signed packed little-endian.
    unsafe {
        let status = AudioQueueNewOutput(
            &format,
            output,
            (callback as *const Callback).cast_mut().cast(),
            ptr::null(),
            ptr::null(),
            0,
            &mut queue,
        );
        if status != 0 {
            return Err(status);
        }
        for _ in 0..BUFFER_COUNT {
            let mut buffer = ptr::null_mut();
            let status = AudioQueueAllocateBuffer(queue, (BUFFER_FRAMES * 4) as u32, &mut buffer);
            if status != 0 {
                // No output was started, hence no callback can still be running.
                AudioQueueDispose(queue, 1);
                while callback.free.pop().is_some() {}
                return Err(status);
            }
            assert!(callback.free.push(FreeBuffer(buffer)).is_ok(), "three initial audio buffers");
        }
    }
    Ok(queue)
}
fn prime(queue: Queue, callback: &Callback) -> Result<bool, i32> {
    let mut enqueued = false;
    for _ in 0..BUFFER_COUNT {
        let Some(FreeBuffer(buffer)) = callback.free.pop() else { break };
        // SAFETY: a free buffer is exclusively owned until enqueue. The first
        // silent buffer schedules the callback that consumes admitted commands.
        unsafe {
            ptr::write_bytes((*buffer).data.cast::<u8>(), 0, BUFFER_FRAMES * 4);
            (*buffer).size = (BUFFER_FRAMES * 4) as u32;
            let status = AudioQueueEnqueueBuffer(queue, buffer, 0, ptr::null());
            if status != 0 {
                return Err(status);
            }
        }
        enqueued = true;
    }
    // SAFETY: the owning device thread alone starts this live queue.
    if enqueued {
        let status = unsafe { AudioQueueStart(queue, ptr::null()) };
        if status != 0 {
            return Err(status);
        }
    }
    Ok(enqueued)
}
unsafe extern "C" fn output(user: *mut c_void, queue: Queue, buffer: *mut Buffer) {
    // SAFETY: AudioQueue returns the registered live callback and its owned buffer.
    let callback = unsafe { &*user.cast::<Callback>() };
    if callback.shared.quit.load(Ordering::Acquire)
        || callback.shared.failed.load(Ordering::Acquire)
    {
        return;
    }
    if callback.entered.compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed).is_err()
    {
        park(callback, buffer);
        return;
    }
    // SAFETY: entered owns the mixer and AudioQueue grants this buffer until
    // reenqueue. Allocation holds BUFFER_FRAMES interleaved stereo PCM16 frames.
    let playing = unsafe {
        let samples =
            std::slice::from_raw_parts_mut((*buffer).data.cast::<i16>(), BUFFER_FRAMES * 2);
        (&mut *callback.mixer.get()).render(&callback.shared, samples)
    };
    callback.entered.store(false, Ordering::Release);
    if !playing {
        park(callback, buffer);
        return;
    }
    // SAFETY: this callback owns buffer and returns it to the live output queue.
    if unsafe { AudioQueueEnqueueBuffer(queue, buffer, 0, ptr::null()) } != 0 {
        callback.shared.failed.store(true, Ordering::Release);
        callback.shared.notify();
    }
}
fn park(callback: &Callback, buffer: *mut Buffer) {
    assert!(callback.free.push(FreeBuffer(buffer)).is_ok(), "each audio buffer has one owner");
    // The last return wakes the owner to stop idle device processing. A command
    // racing the final mixer read also needs a wake, even before all buffers park.
    if callback.free.len() == BUFFER_COUNT || !callback.shared.commands.is_empty() {
        callback.shared.notify();
    }
}
