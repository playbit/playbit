//! One bounded turn snapshot. Payloads keep delivery order; a separate index groups UI targets.
extern crate alloc;

use super::{Event, EventKind, ffi};
use crate::abi::syscall::{
    Error, FrameInput, KeyInput, MAX_EVENT_BYTES, MAX_QUEUED_EVENT_BYTES, MAX_QUEUED_EVENTS,
    OPTIONAL_EVENT_KIND_BIT, PointerInput,
};
use alloc::vec::Vec;
use core::{ops::Range, ptr::NonNull};

#[derive(Default)]
pub(crate) struct EventBuffer {
    records: Vec<Record>,
    bytes: Vec<u8>,
    ui: Vec<(u32, usize)>,
    terminal: Option<Error>,
    pub(super) trace_generation: u64,
}

struct Record {
    generation: u64,
    kind: EventKind,
    target: u32,
    data: Range<usize>,
}

const _: () = assert!(
    MAX_QUEUED_EVENTS * (core::mem::size_of::<Record>() + core::mem::size_of::<(u32, usize)>())
        <= 16 * 1024
);

impl EventBuffer {
    pub(super) fn read(&mut self, raw: NonNull<ffi::Context>) -> Result<(), Error> {
        if let Some(error) = self.terminal {
            return Err(error);
        }
        self.records.clear();
        self.ui.clear();

        match self.read_all(raw) {
            Ok(()) => Ok(()),
            Err(Error::Unsupported) => {
                self.terminal = Some(Error::Unsupported);
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

    pub(super) fn status(&self) -> Result<(), Error> {
        self.terminal.map_or(Ok(()), Err)
    }

    fn read_all(&mut self, raw: NonNull<ffi::Context>) -> Result<(), Error> {
        let mut start = 0;
        let mut consumed = 0;
        let mut charged = 0;
        while consumed < MAX_QUEUED_EVENTS {
            let remaining = MAX_QUEUED_EVENT_BYTES - charged;
            if remaining < 20 {
                break;
            }
            let capacity = (self.bytes.len() - start).min(MAX_EVENT_BYTES).min(remaining - 20);
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
            if length + 20 > remaining {
                break;
            }
            if length > capacity {
                let size = (start + length).max(self.bytes.len() * 2).min(MAX_QUEUED_EVENT_BYTES);
                self.bytes.reserve_exact(size - self.bytes.len());
                self.bytes.resize(size, 0);
                continue;
            }
            consumed += 1;
            charged += length + 20;
            let Some(kind) = event_kind(header.kind)? else {
                continue;
            };
            if kind == EventKind::TraceConfig {
                if length != 8 || header.target != 0 {
                    return Err(Error::InvalidArgument);
                }
                self.trace_generation = u64::from_le_bytes(
                    self.bytes[start..start + 8].try_into().expect("config extent"),
                );
                continue;
            }
            if is_ui(kind) {
                validate_ui(kind, header.target, &self.bytes[start..start + length])?;
                self.ui.push((header.target, self.records.len()));
            }
            self.records.push(Record {
                generation: header.generation,
                kind,
                target: header.target,
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
            data: &self.bytes[record.data.clone()],
        }
    }
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
        EventKind::Pointer => PointerInput::decode(data).map(|_| ()),
        EventKind::Key => KeyInput::decode(data).map(|_| ()),
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
        unknown
            if unknown & OPTIONAL_EVENT_KIND_BIT != 0
                && unknown & !OPTIONAL_EVENT_KIND_BIT > EventKind::TraceConfig as u32 =>
        {
            return Ok(None);
        }
        _ => return Err(Error::Unsupported),
    }))
}
