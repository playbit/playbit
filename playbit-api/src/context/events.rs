//! One bounded turn snapshot. Payloads keep delivery order; a separate index groups UI targets.
extern crate alloc;

use super::{Context, Event, EventKind, PollError, ffi, trace::TraceBuffer};
use crate::abi::syscall::{
    EVENT_QUEUE_OVERHEAD, Error, FrameInput, MAX_EVENT_BYTES, MAX_QUEUED_EVENT_BYTES,
    MAX_QUEUED_EVENTS, OPTIONAL_EVENT_KIND_BIT,
};
use alloc::vec::Vec;
use core::{ops::Range, ptr::NonNull};

#[derive(Default)]
pub(crate) struct EventBuffer {
    records: Vec<Record>,
    bytes: Vec<u8>,
    ui: Vec<(u32, usize)>,
}

struct Record {
    generation: u64,
    kind: EventKind,
    target: u32,
    object_id: u32,
    data: Range<usize>,
}

const _: () = assert!(
    MAX_QUEUED_EVENTS * (core::mem::size_of::<Record>() + core::mem::size_of::<(u32, usize)>())
        <= 16 * 1024
);

impl EventBuffer {
    pub(super) fn read(
        &mut self,
        raw: NonNull<ffi::Context>,
        trace: &mut TraceBuffer,
        event_error: &mut Option<Error>,
    ) -> Result<(), Error> {
        if let Some(error) = *event_error {
            return Err(error);
        }
        self.records.clear();
        self.ui.clear();

        match self.read_all(raw, trace) {
            Ok(()) => Ok(()),
            Err(Error::Unsupported) => {
                *event_error = Some(Error::Unsupported);
                self.ui.sort_unstable();
                // Deliver admitted events once before the wrapper reports the terminal status.
                if self.records.is_empty() { Err(Error::Unsupported) } else { Ok(()) }
            }
            Err(error) => {
                self.records.clear();
                self.ui.clear();
                Err(error)
            }
        }
    }

    fn read_all(
        &mut self,
        raw: NonNull<ffi::Context>,
        trace: &mut TraceBuffer,
    ) -> Result<(), Error> {
        let mut start = 0;
        let mut consumed = 0;
        let mut charged = 0;
        while consumed < MAX_QUEUED_EVENTS {
            let remaining = MAX_QUEUED_EVENT_BYTES - charged;
            if remaining < EVENT_QUEUE_OVERHEAD {
                break;
            }
            let capacity = (self.bytes.len() - start)
                .min(MAX_EVENT_BYTES)
                .min(remaining - EVENT_QUEUE_OVERHEAD);
            let mut header = ffi::Event::default();
            // SAFETY: the callback owns raw; initialized header and payload are disjoint outputs.
            let length = unsafe {
                ffi::pb_event_poll(
                    raw.as_ptr(),
                    &mut header,
                    self.bytes[start..start + capacity].as_mut_ptr(),
                    capacity as u32,
                )
            };
            if length == Error::WouldBlock as i64 {
                break;
            }
            if length < 0 {
                return Err(Error::try_from(length).unwrap_or(Error::Internal));
            }
            let length = length as usize;
            if length > MAX_EVENT_BYTES {
                return Err(Error::LimitExceeded);
            }
            // Draining can admit deferred host notifications. Leave overflow queued for next turn.
            if length + EVENT_QUEUE_OVERHEAD > remaining {
                break;
            }
            if length > capacity {
                let size = (start + length).max(self.bytes.len() * 2).min(MAX_QUEUED_EVENT_BYTES);
                self.bytes.reserve_exact(size - self.bytes.len());
                self.bytes.resize(size, 0);
                continue;
            }
            consumed += 1;
            charged += length + EVENT_QUEUE_OVERHEAD;
            let Some(kind) = classify(&header, &self.bytes[start..start + length], trace)? else {
                continue;
            };
            if is_ui(kind) {
                self.ui.push((header.target, self.records.len()));
            }
            self.records.push(Record {
                generation: header.generation,
                kind,
                target: header.target,
                object_id: header.object_id,
                data: start..start + length,
            });
            start += length;
        }
        self.ui.sort_unstable();
        Ok(())
    }

    pub(crate) fn raw_events(
        &self,
    ) -> impl DoubleEndedIterator<Item = Event<'_>> + ExactSizeIterator {
        self.records.iter().map(|record| self.event(record))
    }

    pub(crate) fn for_target(&self, target: u32) -> impl DoubleEndedIterator<Item = Event<'_>> {
        let start = self.ui.partition_point(|&(id, _)| id < target);
        let end = self.ui.partition_point(|&(id, _)| id <= target);
        self.ui[start..end].iter().map(|&(_, index)| self.event(&self.records[index]))
    }

    pub(crate) fn click_indices(&self, target: u32) -> impl DoubleEndedIterator<Item = usize> {
        let start = self.ui.partition_point(|&(id, _)| id < target);
        let end = self.ui.partition_point(|&(id, _)| id <= target);
        self.ui[start..end].iter().filter_map(|&(_, index)| {
            (self.records[index].kind == EventKind::Click).then_some(index)
        })
    }

    pub(crate) fn selected_clicks(
        &self,
        selected: [bool; MAX_QUEUED_EVENTS],
    ) -> impl DoubleEndedIterator<Item = u32> + '_ {
        self.records
            .iter()
            .enumerate()
            .filter_map(move |(index, record)| selected[index].then_some(record.target))
    }

    fn event<'a>(&'a self, record: &Record) -> Event<'a> {
        Event {
            generation: record.generation,
            kind: record.kind,
            target: record.target,
            object_id: record.object_id,
            data: &self.bytes[record.data.clone()],
        }
    }
}

impl Context<'_> {
    /// Consume one new event, waiting until an event arrives or the absolute
    /// `clock_read()` deadline in nanoseconds. Zero polls immediately and
    /// `u64::MAX` waits indefinitely. At expiry an empty queue returns
    /// `PollError::Host(Error::WouldBlock)`. A size probe leaves the event queued.
    /// Process `raw_events()` first to handle the callback's initial snapshot.
    /// Include `event.poll.deadline` in the package's `required_host_features`.
    pub fn poll_event_deadline<'buf>(
        &mut self,
        deadline_ns: u64,
        payload: &'buf mut [u8],
    ) -> Result<Event<'buf>, PollError> {
        self.event_status().map_err(PollError::Host)?;
        let capacity = payload.len().min(MAX_EVENT_BYTES);
        loop {
            // A registered callback can remain parked indefinitely. Release dropped
            // File resources before waiting, including after an internal event
            // has freed queue space for a previously deferred disposal.
            if let Some(files) = self.files.clone() {
                files.flush(self).map_err(PollError::Host)?;
            }
            let mut header = ffi::Event::default();
            // SAFETY: both outputs are disjoint live borrows until the poll returns.
            let result = unsafe {
                ffi::pb_event_poll_deadline(
                    self.raw.as_ptr(),
                    &mut header,
                    payload.as_mut_ptr(),
                    capacity as u32,
                    deadline_ns,
                )
            };
            if result < 0 {
                return Err(PollError::Host(Error::try_from(result).unwrap_or(Error::Internal)));
            }
            if result > MAX_EVENT_BYTES as i64 {
                return Err(PollError::Host(Error::LimitExceeded));
            }
            let length = result as usize;
            if length > capacity {
                return Err(PollError::BufferTooSmall(length));
            }
            let kind = match classify(&header, &payload[..length], self.trace) {
                Ok(Some(kind)) => kind,
                Ok(None) => continue,
                Err(error) => {
                    if error == Error::Unsupported {
                        *self.event_error = Some(error);
                    }
                    return Err(PollError::Host(error));
                }
            };
            return Ok(Event {
                generation: header.generation,
                kind,
                target: header.target,
                object_id: header.object_id,
                data: &payload[..length],
            });
        }
    }
}

fn classify(
    header: &ffi::Event,
    data: &[u8],
    trace: &mut TraceBuffer,
) -> Result<Option<EventKind>, Error> {
    let Some(kind) = event_kind(header.kind)? else { return Ok(None) };
    if kind == EventKind::TraceConfig {
        if header.target != 0 {
            return Err(Error::InvalidArgument);
        }
        trace.configure(u64::from_le_bytes(data.try_into().map_err(|_| Error::InvalidArgument)?));
        return Ok(None);
    }
    if kind == EventKind::IdentityChanged && (header.target != 0 || !data.is_empty()) {
        return Err(Error::InvalidArgument);
    }
    if crate::abi::input::Input::family(kind) != 0 {
        if header.object_id == 0 {
            return Err(Error::InvalidArgument);
        }
        crate::abi::input::Input::decode(kind, data)?;
    }
    if is_ui(kind) {
        validate_ui(kind, header.target, data)?;
    }
    Ok(Some(kind))
}

fn is_ui(kind: EventKind) -> bool {
    matches!(
        kind,
        EventKind::Click
            | EventKind::Submit
            | EventKind::TextChanged
            | EventKind::Pointer
            | EventKind::Key
            | EventKind::Frame
    )
}

fn validate_ui(kind: EventKind, target: u32, data: &[u8]) -> Result<(), Error> {
    if kind == EventKind::Submit && data.len() > crate::abi::text::MAX_TEXT_BYTES {
        return Err(Error::LimitExceeded);
    }
    if target == 0 {
        return Err(Error::InvalidArgument);
    }
    match kind {
        EventKind::Click if data.is_empty() => Ok(()),
        EventKind::TextChanged => crate::abi::text::TextInput::decode(data).map(|_| ()),
        EventKind::Submit => {
            core::str::from_utf8(data).map(|_| ()).map_err(|_| Error::InvalidArgument)
        }
        EventKind::Pointer => crate::abi::syscall::PointerInput::decode(data).map(|_| ()),
        EventKind::Key => crate::abi::syscall::KeyInput::decode(data).map(|_| ()),
        EventKind::Frame => FrameInput::decode(data).map(|_| ()),
        _ => Err(Error::InvalidArgument),
    }
}

fn event_kind(value: u32) -> Result<Option<EventKind>, Error> {
    Ok(Some(match value {
        1 => EventKind::Started,
        2 => EventKind::Click,
        3 => EventKind::Submit,
        4 => EventKind::DataChanged,
        5 => EventKind::PresenceChanged,
        6 => EventKind::UpdateAvailable,
        7 => EventKind::ObjectCreationReady,
        8 => EventKind::FileCopyReady,
        9 => EventKind::FileSelectionReady,
        10 => EventKind::Wake,
        11 => EventKind::Pointer,
        12 => EventKind::Key,
        13 => EventKind::Frame,
        14 => EventKind::TextChanged,
        15 => EventKind::SurfaceChanged,
        16 => EventKind::DataReady,
        17 => EventKind::VolumesChanged,
        18 => EventKind::ChannelMessage,
        19 => EventKind::ChannelStatus,
        20 => EventKind::TraceConfig,
        22 => EventKind::WindowPointer,
        23 => EventKind::WindowKeyboard,
        24 => EventKind::WindowScroll,
        25 => EventKind::WindowGesture,
        26 => EventKind::WindowGamepad,
        value if value == EventKind::IdentityChanged as u32 => EventKind::IdentityChanged,
        unknown
            if unknown & OPTIONAL_EVENT_KIND_BIT != 0
                && unknown & !OPTIONAL_EVENT_KIND_BIT > EventKind::WindowGamepad as u32 =>
        {
            return Ok(None);
        }
        _ => return Err(Error::Unsupported),
    }))
}
