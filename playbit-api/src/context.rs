//! Typed app operations, using direct C calls natively and scalar imports on wasm32.
//! Native contexts borrow a lifecycle callback; Wasm authority is the import caller.
#![allow(
    unsafe_code,
    reason = "This SDK adapter passes live Rust slices and initialized output records to target-specific C ABI operations."
)]
use crate::abi::{
    operations as ffi,
    syscall::{Error, EventKind},
};
extern crate alloc;

use crate::ui::ViewEncoder;
use alloc::vec::Vec;
use core::{marker::PhantomData, ptr::NonNull};

mod audio;
mod events;
pub use audio::{Audio, Gain, Sound, SoundInstance, Timestamp};
pub(crate) use events::EventBuffer;

mod session;
pub use session::{Channel, ChannelMessage, Session, SessionInfo};
mod trace;
pub use trace::TraceSpan;
mod authenticated;
pub use authenticated::{AuthenticatedField, AuthenticatedReadError, Principal, Provenance};
mod data;
mod environment;
mod file_ownership;
mod files;
pub use file_ownership::{File, FileClose, FileList, FileOperation, FileResources};
mod bloblog;
mod kv;
mod pending;
pub use crate::abi::data::{EntryHandle, VolumeRole};
pub use crate::abi::environment::{HostLimit, HostPlatform, Participant, Surface};
pub use crate::abi::files::FileReference;
pub use crate::abi::pending::OperationHandle;
pub use environment::{Arguments, Capabilities, MetadataError};
pub use files::{FileCloseResult, FileHandle, FileListEntry, FileListHandle, FileStatus};
pub use pending::Completion;
mod schema;
pub use data::{
    Field, FieldChange, Object, Page, PageObject, PageStorage, Query, QueryError, ReadError, Value,
    ValueRef, Volume, VolumeHandle, Volumes,
};
pub use schema::{FieldDefinition, Schema, SchemaField, SchemaType, TypeDefinition, ValueType};

/// Reusable SDK state, owned per instance by registered apps.
/// Direct ABI callers retain one state across callbacks.
#[derive(Default)]
pub struct ContextState {
    events: EventBuffer,
    event_error: Option<Error>,
    scene: Vec<u8>,
    trace: trace::TraceBuffer,
}

impl ContextState {
    /// Begin a standalone scene using this state's latest event snapshot.
    pub fn view(&mut self) -> ViewEncoder<'_> {
        ViewEncoder::new(&mut self.scene, &self.events)
    }

    #[cfg(test)]
    pub(crate) fn raw_events(
        &self,
    ) -> impl DoubleEndedIterator<Item = Event<'_>> + ExactSizeIterator {
        self.events.raw_events()
    }
}

pub struct Context<'a> {
    raw: NonNull<ffi::Context>,
    files: Option<FileResources>,
    events: &'a EventBuffer,
    event_error: &'a mut Option<Error>,
    trace: &'a mut trace::TraceBuffer,
    _borrow: PhantomData<&'a mut ffi::Context>,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PollError {
    Host(Error),
    BufferTooSmall(usize),
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Event<'a> {
    pub generation: u64,
    pub kind: EventKind,
    pub target: u32,
    pub data: &'a [u8],
}
fn status(value: i64) -> Result<(), Error> {
    if value == 0 { Ok(()) } else { Err(Error::try_from(value).unwrap_or(Error::Internal)) }
}
impl<'a> Context<'a> {
    /// Bind operations to this Wasm import caller. This token never represents a host pointer.
    /// Returns a context and encoder sharing this turn's event snapshot.
    #[cfg(target_arch = "wasm32")]
    pub fn guest(state: &'a mut ContextState) -> Result<(Self, ViewEncoder<'a>), Error> {
        let raw = NonNull::new(crate::abi::invocation::CONTEXT_TOKEN as *mut ffi::Context)
            .expect("the local guest token is nonzero");
        state.events.read(raw, &mut state.trace, &mut state.event_error)?;
        let context = Self {
            raw,
            files: None,
            events: &state.events,
            event_error: &mut state.event_error,
            trace: &mut state.trace,
            _borrow: PhantomData,
        };
        let view = ViewEncoder::new(&mut state.scene, &state.events);
        Ok((context, view))
    }

    /// Borrow a native callback and snapshot its queued events into reusable storage.
    /// Returns a context and encoder sharing this turn's event snapshot.
    ///
    /// # Safety
    /// The pointer must name a live context on its owning callback thread for
    /// all of 'a. The wrapper and any access through it must not escape that
    /// callback. The same borrow must not be concurrently used through raw calls.
    #[cfg(not(target_arch = "wasm32"))]
    pub unsafe fn from_raw(
        raw: *mut ffi::Context,
        state: &'a mut ContextState,
    ) -> Result<(Self, ViewEncoder<'a>), Error> {
        let raw = NonNull::new(raw).ok_or(Error::InvalidArgument)?;
        state.events.read(raw, &mut state.trace, &mut state.event_error)?;
        let context = Self {
            raw,
            files: None,
            events: &state.events,
            event_error: &mut state.event_error,
            trace: &mut state.trace,
            _borrow: PhantomData,
        };
        let view = ViewEncoder::new(&mut state.scene, &state.events);
        Ok((context, view))
    }

    /// All events from the start of this turn, in delivery order. Queries do not consume them.
    pub fn raw_events(
        &self,
    ) -> impl DoubleEndedIterator<Item = Event<'a>> + ExactSizeIterator + 'a {
        self.events.raw_events()
    }

    /// Check after handling this snapshot. Unsupported mandatory events stop
    /// admission; known events before that boundary remain available once.
    /// Direct callers must return this error to their host after processing them.
    pub fn event_status(&self) -> Result<(), Error> {
        self.event_error.map_or(Ok(()), Err)
    }

    /// Log a message.
    ///
    /// # Panics
    /// Panics if the message is too large for the ABI or the host rejects it.
    pub fn log(&mut self, message: &str) {
        let length = u32::try_from(message.len()).expect("log message length fits in u32");

        // SAFETY: the context is live; the immutable string spans this call.
        status(unsafe { ffi::pb_log(self.raw.as_ptr(), message.as_ptr(), length) })
            .expect("host accepts log messages");
    }

    /// Declare whether closing the last window quits this instance. The choice
    /// is immutable; explicit Quit always closes admission and drains work.
    pub fn quit_on_last_window(&mut self, intent: bool) -> Result<(), Error> {
        // SAFETY: the context is live and the Boolean is canonical.
        status(unsafe { ffi::pb_quit_on_last_window(self.raw.as_ptr(), u32::from(intent)) })
    }
    pub fn submit_view(&mut self, scene: &[u8]) -> Result<(), Error> {
        let length = u32::try_from(scene.len()).map_err(|_| Error::LimitExceeded)?;
        // SAFETY: the scene is borrowed and the host retains no caller pointer.
        status(unsafe { ffi::pb_view_submit(self.raw.as_ptr(), scene.as_ptr(), length) })
    }
    pub fn identity(&mut self) -> Result<ffi::Identity, Error> {
        let mut value = ffi::Identity::default();
        // SAFETY: value is initialized, aligned and exclusively borrowed.
        status(unsafe { ffi::pb_identity(self.raw.as_ptr(), &mut value) })?;
        if value.reserved != 0 || value.flags & !3 != 0 {
            return Err(Error::Internal);
        }
        Ok(value)
    }
    /// Read the host monotonic clock in nanoseconds, including for event-poll deadlines.
    pub fn clock_read(&self) -> u64 {
        // SAFETY: this call borrows the live context and takes no output pointers.
        unsafe { ffi::pb_clock_read(self.raw.as_ptr()) }
    }

    pub fn wake_after(&mut self, delay_ms: u32) -> Result<(), Error> {
        // SAFETY: only the live callback context is borrowed.
        status(unsafe { ffi::pb_wake_after(self.raw.as_ptr(), delay_ms) })
    }
    pub fn defer_update(&mut self, generation: u64) -> Result<(), Error> {
        // SAFETY: only the live callback context is borrowed.
        status(unsafe { ffi::pb_update_defer(self.raw.as_ptr(), generation) })
    }
}

fn range<T>(values: &[T], range: crate::abi::data::Range) -> Result<&[T], Error> {
    let start = range.offset as usize;
    let end = start.checked_add(range.count as usize).ok_or(Error::Internal)?;
    values.get(start..end).ok_or(Error::Internal)
}
fn buffer<T: crate::abi::record::Element>(
    values: &mut [T],
) -> Result<crate::abi::data::Buffer<T>, Error> {
    let capacity = u32::try_from(values.len()).map_err(|_| Error::LimitExceeded)?;
    for value in values.iter_mut() {
        value.initialize();
    }
    Ok(crate::abi::data::Buffer::new(values.as_mut_ptr(), capacity))
}
