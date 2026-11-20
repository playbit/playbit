//! Guest-owned bounded event buffering. Tokens can survive callbacks and end in
//! any order; the host never maintains a guest span stack.
use super::{Context, Error, ffi, status};
use crate::abi::trace::{self, TraceEvent};
extern crate alloc;
use alloc::boxed::Box;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TraceSpan {
    timestamp: u64,
    generation: u64,
}
impl TraceSpan {
    pub fn timestamp(self) -> u64 {
        self.timestamp
    }
}

#[derive(Default)]
pub(super) struct TraceBuffer {
    bytes: Option<Box<[u8; trace::BUFFER_BYTES]>>,
    used: usize,
    count: u64,
    generation: u64,
    lost: u64,
    reported_loss: u64,
    last_t: u64,
}
impl TraceBuffer {
    pub(super) fn configure(&mut self, generation: u64) {
        if self.generation != generation {
            // Stop never waits for a paused guest. Its unflushed tail belongs to
            // the previous generation and cannot be relabeled into this one.
            self.used = 0;
            self.count = 0;
            self.lost = 0;
            self.reported_loss = 0;
            self.generation = generation;
        }
    }
    fn refuse(&mut self, count: u64) -> Result<(), Error> {
        self.lost = self.lost.checked_add(count).ok_or(Error::LimitExceeded)?;
        Ok(())
    }
    fn flush(&mut self, raw: *mut ffi::Context) -> Result<(), Error> {
        if self.generation == 0 || (self.count == 0 && self.lost == self.reported_loss) {
            return Ok(());
        }
        let bytes = self.bytes.get_or_insert_with(|| Box::new([0; trace::BUFFER_BYTES]));
        // SAFETY: the live callback lends a complete byte batch; no pointer is retained.
        let result = status(unsafe {
            ffi::pb_trace_record(raw, bytes.as_ptr(), self.used as u32, self.generation, self.lost)
        });
        let count = self.count;
        self.used = 0;
        self.count = 0;
        self.reported_loss = self.lost;
        if result.is_err() {
            self.refuse(count)?;
        }
        result
    }
    fn emit(
        &mut self,
        raw: *mut ffi::Context,
        mut header: TraceEvent,
        metadata: &[u8],
    ) -> Result<TraceSpan, Error> {
        if self.generation == 0 {
            return Err(Error::Unavailable);
        }
        let size = match trace::event_bytes(metadata.len()) {
            Ok(size) => size,
            Err(error) => {
                self.refuse(1)?;
                return Err(error);
            }
        };
        if self.used + size > trace::BUFFER_BYTES {
            let _ = self.flush(raw);
        }
        if self.used + size > trace::BUFFER_BYTES {
            self.refuse(1)?;
            return Err(Error::LimitExceeded);
        }
        // SAFETY: raw belongs to the live callback.
        let now = unsafe { ffi::pb_clock_read(raw) };
        let Some(timestamp) = trace::make_timestamp(&mut self.last_t, now) else {
            self.refuse(1)?;
            return Err(Error::LimitExceeded);
        };
        header.timestamp = timestamp;
        header.metadata_len = metadata.len() as u32;
        let bytes = self.bytes.get_or_insert_with(|| Box::new([0; trace::BUFFER_BYTES]));
        let event = &mut bytes[self.used..self.used + size];
        event.fill(0);
        trace::encode_header(header, event)?;
        event[trace::HEADER_BYTES..][..metadata.len()].copy_from_slice(metadata);
        self.used += size;
        self.count += 1;
        let token = TraceSpan { timestamp, generation: self.generation };
        if self.used >= trace::BUFFER_BYTES * 3 / 4 {
            let _ = self.flush(raw);
        }
        Ok(token)
    }
}
impl Context<'_> {
    pub fn trace_enabled(&self) -> bool {
        self.trace.generation != 0
    }
    /// Begin an owned span. Metadata is already-encoded UTF-8 JSON object bytes.
    pub fn trace_begin(
        &mut self,
        name: u16,
        parent: Option<TraceSpan>,
        metadata: &[u8],
    ) -> Result<TraceSpan, Error> {
        self.trace_event(trace::START, name, 0, parent, metadata)
    }
    pub fn trace_end(&mut self, span: TraceSpan, metadata: &[u8]) -> Result<(), Error> {
        self.trace_event(trace::END, 0, 0, Some(span), metadata).map(|_| ())
    }
    pub fn trace_instant(
        &mut self,
        name: u16,
        value: u64,
        parent: Option<TraceSpan>,
        metadata: &[u8],
    ) -> Result<(), Error> {
        self.trace_event(trace::INSTANT, name, value, parent, metadata).map(|_| ())
    }
    pub fn trace_mark(&mut self, name: u16, value: u64) -> Result<(), Error> {
        self.trace_instant(name, value, None, &[])
    }
    pub fn trace_flush(&mut self) -> Result<(), Error> {
        self.trace.flush(self.raw.as_ptr())
    }
    fn trace_event(
        &mut self,
        kind: u8,
        name: u16,
        value: u64,
        reference: Option<TraceSpan>,
        metadata: &[u8],
    ) -> Result<TraceSpan, Error> {
        if let Some(reference) = reference
            && reference.generation != self.trace.generation
        {
            self.trace.refuse(1)?;
            return Err(Error::StaleHandle);
        }
        self.trace.emit(
            self.raw.as_ptr(),
            TraceEvent {
                kind,
                name,
                value,
                reference_eid: reference.map_or(0, |span| span.timestamp),
                ..TraceEvent::default()
            },
            metadata,
        )
    }
}
