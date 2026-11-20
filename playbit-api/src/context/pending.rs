//! Typed pending work: wait for host-ready events; never spin or poll on a timer.
use super::{Context, Error, Event, EventKind, FileReference, OperationHandle, status};
use crate::abi::{
    pending as ffi,
    syscall::{Id, Reader},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Completion<T> {
    Pending,
    Ready(T),
}
pub(super) fn admitted(result: i64) -> Result<OperationHandle, Error> {
    let handle = OperationHandle(handle_result(result)?);
    if handle.generation() == 0 || handle.slot() == 0 {
        return Err(Error::Internal);
    }
    Ok(handle)
}
pub(super) fn handle_result(result: i64) -> Result<u64, Error> {
    if result <= 0 {
        return Err(Error::try_from(result).unwrap_or(Error::Internal));
    }
    Ok(result as u64)
}
impl Event<'_> {
    /// Decode the data format of an operation-ready event. The handle belongs
    /// to the current guest table; the event epoch is checked separately.
    pub fn operation(&self) -> Result<OperationHandle, Error> {
        if !matches!(
            self.kind,
            EventKind::ObjectCreationReady
                | EventKind::FileCopyReady
                | EventKind::FileSelectionReady
                | EventKind::DataReady
        ) || self.target != 0
        {
            return Err(Error::InvalidArgument);
        }
        let mut reader = Reader::new(self.data)?;
        let handle = reader.operation_handle()?;
        reader.finish()?;
        if handle.generation == 0 || handle.generation > i32::MAX as u32 || handle.slot == 0 {
            return Err(Error::InvalidArgument);
        }
        Ok(OperationHandle::new(handle.generation, handle.slot))
    }
}
impl Context<'_> {
    pub fn begin_create_object(
        &mut self,
        volume: super::EntryHandle,
        type_id: Id,
        changes: &[super::FieldChange<'_>],
    ) -> Result<OperationHandle, Error> {
        let count = u32::try_from(changes.len()).map_err(|_| Error::LimitExceeded)?;
        // SAFETY: the context and borrowed input spans remain live and stable for this call.
        let result = unsafe {
            ffi::pb_object_create_begin(
                self.raw.as_ptr(),
                &volume,
                &type_id,
                changes.as_ptr().cast(),
                count,
                core::mem::size_of::<crate::abi::data::FieldInput>() as u32,
            )
        };
        admitted(result)
    }
    pub fn begin_file_copy(
        &mut self,
        source: FileReference,
        destination: FileReference,
    ) -> Result<OperationHandle, Error> {
        // SAFETY: the context and borrowed input spans remain live and stable for this call.
        let result = unsafe { ffi::pb_file_copy_begin(self.raw.as_ptr(), &source, &destination) };
        admitted(result)
    }
    pub fn begin_file_selection(&mut self) -> Result<OperationHandle, Error> {
        // SAFETY: the current callback context remains live for this call.
        let result = unsafe { ffi::pb_file_select_begin(self.raw.as_ptr()) };
        admitted(result)
    }
    /// Terminal success or error consumes the operation. Pending retains it.
    pub fn poll_create_object(&mut self, handle: OperationHandle) -> Result<Completion<Id>, Error> {
        let mut output = [0; 16];
        // SAFETY: initialized ID is an exclusive live output.
        let result = unsafe { ffi::pb_object_create_poll(self.raw.as_ptr(), handle, &mut output) };
        if result == Error::WouldBlock as i64 {
            return Ok(Completion::Pending);
        }
        status(result)?;
        Ok(Completion::Ready(output))
    }
    /// Success names the destination entry originally submitted to copy.
    pub fn poll_file_copy(
        &mut self,
        handle: OperationHandle,
    ) -> Result<Completion<[u8; 15]>, Error> {
        let mut output = [0; 15];
        // SAFETY: initialized ID is an exclusive live output.
        let result = unsafe { ffi::pb_file_copy_poll(self.raw.as_ptr(), handle, &mut output) };
        if result == Error::WouldBlock as i64 {
            return Ok(Completion::Pending);
        }
        status(result)?;
        Ok(Completion::Ready(output))
    }
    /// Ready(None) is cancellation; Ready(Some(file)) conveys no new authority.
    pub fn poll_file_selection(
        &mut self,
        handle: OperationHandle,
    ) -> Result<Completion<Option<FileReference>>, Error> {
        let mut output = ffi::Selection::default();
        // SAFETY: entire result, including the canceled case, is a writable live record.
        let result = unsafe { ffi::pb_file_select_poll(self.raw.as_ptr(), handle, &mut output) };
        if result == Error::WouldBlock as i64 {
            return Ok(Completion::Pending);
        }
        status(result)?;
        if output.reserved != 0 || output.present > 1 {
            return Err(Error::Internal);
        }
        if output.present == 0 {
            if output.file() != FileReference::default() {
                return Err(Error::Internal);
            }
            return Ok(Completion::Ready(None));
        }
        if output.file().volume_handle().slot() == 0
            || output.file().reserved != 0
            || output.file().entry_id == [0; 15]
        {
            return Err(Error::Internal);
        }
        Ok(Completion::Ready(Some(output.file())))
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn scalar_handles_preserve_bits_and_reject_errors() {
        // Results are decoded before narrowing, including values wider than JS numbers.
        for value in [i32::MAX as i64 + 1, (1i64 << 53) + 1, i64::MAX] {
            assert_eq!(super::handle_result(value), Ok(value as u64));
        }
        assert_eq!(super::handle_result(0), Err(super::Error::Internal));
        assert_eq!(super::handle_result(-8), Err(super::Error::StaleHandle));
        assert_eq!(super::handle_result(i64::MIN + u32::MAX as i64), Err(super::Error::Internal));
    }
}
