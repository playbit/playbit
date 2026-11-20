//! Async append/read helpers borrow input and reuse caller-owned result storage.
use super::{Completion, Context, Error, buffer, status};
use crate::abi::{bloblog as ffi, data::EntryHandle, pending::OperationHandle};

impl Context<'_> {
    /// Append one batch to an existing log. Admission copies the payloads;
    /// completion acknowledges AP's durable commit. Repeat an uncertain call with
    /// the same ID and payload; a different ID is a new append.
    pub fn begin_bloblog_append(
        &mut self,
        entry: EntryHandle,
        append_id: &[u8; 16],
        blobs: &[&[u8]],
    ) -> Result<OperationHandle, Error> {
        if !entry.valid() {
            return Err(Error::InvalidArgument);
        }
        ffi::validate_lengths(blobs.iter().map(|blob| blob.len()))?;
        let mut inputs = [ffi::Blob::default(); ffi::MAX_BLOBS];
        for (input, blob) in inputs.iter_mut().zip(blobs) {
            input.data = blob.as_ptr();
            input.length = blob.len() as u32;
        }
        // SAFETY: the context and borrowed input spans remain live and stable for this call.
        let result = unsafe {
            ffi::pb_bloblog_append_begin(
                self.raw.as_ptr(),
                entry.volume_handle(),
                entry.entry_id.as_ptr(),
                inputs.as_ptr(),
                blobs.len() as u32,
                core::mem::size_of::<ffi::Blob>() as u32,
                append_id.as_ptr(),
            )
        };
        super::pending::admitted(result)
    }

    /// Read after an opaque local cursor, or from the beginning with `None`.
    /// A cursor from another replica replays the log; deduplicate by entry ID.
    pub fn begin_bloblog_read(
        &mut self,
        entry: EntryHandle,
        cursor: Option<&str>,
    ) -> Result<OperationHandle, Error> {
        if !entry.valid() {
            return Err(Error::InvalidArgument);
        }
        if let Some(cursor) = cursor {
            ffi::cursor(cursor.as_bytes())?;
        }
        let cursor = cursor.unwrap_or_default();
        // SAFETY: entry/cursor stay borrowed until begin copies them.
        let result = unsafe {
            ffi::pb_bloblog_read_begin(
                self.raw.as_ptr(),
                entry.volume_handle(),
                entry.entry_id.as_ptr(),
                cursor.as_ptr(),
                cursor.len() as u32,
            )
        };
        super::pending::admitted(result)
    }

    /// Returns a cursor before the appended batch in caller storage. A short
    /// buffer retains the receipt; retry take rather than repeating the append.
    pub fn take_bloblog_append<'a>(
        &mut self,
        handle: OperationHandle,
        bytes: &'a mut [u8],
    ) -> Result<Completion<&'a str>, Error> {
        let mut output = buffer(bytes)?;
        // SAFETY: count and caller-owned output bytes are disjoint/live.
        let result = unsafe {
            ffi::pb_bloblog_append_take(
                self.raw.as_ptr(),
                handle,
                output.data,
                output.capacity,
                &mut output.count,
            )
        };
        if result == Error::WouldBlock as i64 {
            return Ok(Completion::Pending);
        }
        status(result)?;
        let bytes = bytes.get(..output.count as usize).ok_or(Error::Internal)?;
        Ok(Completion::Ready(ffi::cursor(bytes)?))
    }

    /// Returns a bounded page borrowed from `bytes`. Retain its cursor even
    /// when empty; `more` means another page is available locally.
    pub fn take_bloblog_read<'a>(
        &mut self,
        handle: OperationHandle,
        bytes: &'a mut [u8],
    ) -> Result<Completion<ffi::Page<'a>>, Error> {
        let mut output = buffer(bytes)?;
        // SAFETY: count and caller-owned output bytes are disjoint/live.
        let result = unsafe {
            ffi::pb_bloblog_read_take(
                self.raw.as_ptr(),
                handle,
                output.data,
                output.capacity,
                &mut output.count,
            )
        };
        if result == Error::WouldBlock as i64 {
            return Ok(Completion::Pending);
        }
        status(result)?;
        let bytes = bytes.get(..output.count as usize).ok_or(Error::Internal)?;
        Ok(Completion::Ready(ffi::decode_page(bytes)?))
    }
}
