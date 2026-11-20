//! Single-writer byte queues carry only complete events. Slots are atomic so
//! malformed peer counters never produce a data race. Admission never waits.
use std::sync::atomic::{AtomicU64, Ordering};
pub const MAX_PRODUCERS: usize = 32;
pub const MAX_SOURCES: usize = 8;
pub const QUEUE_BYTES: usize = 16 * 1024;
pub const WORDS: usize = QUEUE_BYTES / 8;
pub const EVENT_MAX_BYTES: usize = 16 + 40 + 4096;
#[repr(C)]
pub struct Queue {
    pub state: AtomicU64,
    pub head: AtomicU64,
    pub tail: AtomicU64,
    pub lost: AtomicU64,
    pub words: [AtomicU64; WORDS],
}
impl Queue {
    pub fn push(&self, prefix: &[u8; 16], event: &[u8]) -> bool {
        assert!(event.len() >= playbit_api::abi::trace::HEADER_BYTES);
        let bytes = prefix.len() + event.len();
        assert!(bytes <= EVENT_MAX_BYTES && bytes.is_multiple_of(8));
        let head = self.head.load(Ordering::Relaxed);
        let tail = self.tail.load(Ordering::Acquire);
        let count = bytes / 8;
        let Some(used) = head.checked_sub(tail) else {
            return false;
        };
        let Some(end) = head.checked_add(count as u64) else {
            return false;
        };
        if used > WORDS as u64 || count as u64 > WORDS as u64 - used {
            self.lost.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        let reference = u64::from_le_bytes(event[8..16].try_into().expect("event reference"));
        let tid = u32::from_le_bytes(prefix[4..8].try_into().expect("trusted thread"));
        for (index, chunk) in
            prefix.as_chunks::<8>().0.iter().chain(event.as_chunks::<8>().0.iter()).enumerate()
        {
            let mut word = u64::from_le_bytes(*chunk);
            if index == 5 && reference != 0 && word as u32 == 0 {
                word |= u64::from(tid);
            }
            self.words[(head as usize + index) % WORDS].store(word, Ordering::Relaxed);
        }
        self.head.store(end, Ordering::Release);
        true
    }
    pub fn pop(&self, output: &mut [u8; EVENT_MAX_BYTES]) -> Result<usize, &'static str> {
        let tail = self.tail.load(Ordering::Relaxed);
        let head = self.head.load(Ordering::Acquire);
        let used = head.checked_sub(tail).ok_or("reversed queue")?;
        if used == 0 {
            return Ok(0);
        }
        if used > WORDS as u64 || used < 7 {
            return Err("invalid queue extent");
        }
        for index in 0..7 {
            output[index * 8..index * 8 + 8].copy_from_slice(
                &self.words[(tail as usize + index) % WORDS].load(Ordering::Relaxed).to_le_bytes(),
            );
        }
        let metadata =
            u32::from_le_bytes(output[44..48].try_into().expect("metadata length")) as usize;
        let length = 16
            + playbit_api::abi::trace::event_bytes(metadata)
                .map_err(|_| "invalid metadata extent")?;
        let count = length / 8;
        if count as u64 > used {
            return Err("partial event publication");
        }
        for index in 7..count {
            output[index * 8..index * 8 + 8].copy_from_slice(
                &self.words[(tail as usize + index) % WORDS].load(Ordering::Relaxed).to_le_bytes(),
            );
        }
        self.tail.store(tail + count as u64, Ordering::Release);
        Ok(length)
    }
}
