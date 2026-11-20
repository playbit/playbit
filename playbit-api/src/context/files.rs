//! Borrowed File calls. Owned SDK resources layer their cleanup on these calls.
use super::{Completion, Context, Error, OperationHandle, PollError, buffer, status};
use crate::abi::files as ffi;
pub use ffi::{FileCloseResult, FileHandle, FileListHandle, FileStatus};
#[derive(Debug, Eq, PartialEq)]
pub struct FileListEntry<'a> {
    pub kind: u32,
    pub name: &'a [u8],
}
impl Context<'_> {
    pub fn begin_open_file(
        &mut self,
        parent: FileHandle,
        path: &[u8],
        mode: u32,
    ) -> Result<OperationHandle, Error> {
        let length = u32::try_from(path.len()).map_err(|_| Error::LimitExceeded)?;
        // SAFETY: the context and borrowed input spans remain live and stable for this call.
        let result = unsafe {
            ffi::pb_file_open_begin(self.raw.as_ptr(), parent, path.as_ptr(), length, mode)
        };
        super::pending::admitted(result)
    }
    pub fn take_open_file(
        &mut self,
        operation: OperationHandle,
    ) -> Result<Completion<FileHandle>, Error> {
        // SAFETY: the current callback context remains live for this call.
        let code = unsafe { ffi::pb_file_open_take(self.raw.as_ptr(), operation) };
        if code == Error::WouldBlock as i64 {
            return Ok(Completion::Pending);
        }
        let output = FileHandle(super::pending::handle_result(code)?);
        Ok(Completion::Ready(output))
    }
    pub fn begin_read_file(
        &mut self,
        file: FileHandle,
        offset: u64,
        length: u32,
    ) -> Result<OperationHandle, Error> {
        // SAFETY: the current callback context remains live for this call.
        let result = unsafe { ffi::pb_file_read_begin(self.raw.as_ptr(), file, offset, length) };
        super::pending::admitted(result)
    }
    pub fn take_read_file<'a>(
        &mut self,
        operation: OperationHandle,
        storage: &'a mut [u8],
    ) -> Result<Completion<&'a [u8]>, PollError> {
        let capacity = storage.len().min(ffi::MAX_FILE_READ as usize);
        let mut output = buffer(&mut storage[..capacity]).map_err(PollError::Host)?;
        // SAFETY: descriptor and exclusive output bytes are disjoint live borrows.
        let code = unsafe {
            ffi::pb_file_read_take(
                self.raw.as_ptr(),
                operation,
                output.data,
                output.capacity,
                &mut output.count,
            )
        };
        if code == Error::WouldBlock as i64 {
            return Ok(Completion::Pending);
        }
        if code == Error::BufferTooSmall as i64 {
            return Err(PollError::BufferTooSmall(output.count as usize));
        }
        status(code).map_err(PollError::Host)?;
        let bytes = storage.get(..output.count as usize).ok_or(PollError::Host(Error::Internal))?;
        Ok(Completion::Ready(bytes))
    }
    pub fn begin_write_file(
        &mut self,
        file: FileHandle,
        offset: u64,
        bytes: &[u8],
    ) -> Result<OperationHandle, Error> {
        let length = u32::try_from(bytes.len()).map_err(|_| Error::LimitExceeded)?;
        // SAFETY: the context and borrowed input spans remain live and stable for this call.
        let result = unsafe {
            ffi::pb_file_write_begin(self.raw.as_ptr(), file, offset, bytes.as_ptr(), length)
        };
        super::pending::admitted(result)
    }
    pub fn take_write_file(&mut self, operation: OperationHandle) -> Result<Completion<()>, Error> {
        // SAFETY: only the current callback context is borrowed.
        let code = unsafe { ffi::pb_file_write_take(self.raw.as_ptr(), operation) };
        if code == Error::WouldBlock as i64 {
            return Ok(Completion::Pending);
        }
        status(code)?;
        Ok(Completion::Ready(()))
    }
    pub fn begin_file_status(&mut self, file: FileHandle) -> Result<OperationHandle, Error> {
        // SAFETY: the current callback context remains live for this call.
        let result = unsafe { ffi::pb_file_status_begin(self.raw.as_ptr(), file) };
        super::pending::admitted(result)
    }
    pub fn take_file_status(
        &mut self,
        operation: OperationHandle,
    ) -> Result<Completion<FileStatus>, Error> {
        let mut output = FileStatus::default();
        // SAFETY: fixed output is initialized and exclusive.
        let code = unsafe { ffi::pb_file_status_take(self.raw.as_ptr(), operation, &mut output) };
        if code == Error::WouldBlock as i64 {
            return Ok(Completion::Pending);
        }
        status(code)?;
        Ok(Completion::Ready(output))
    }
    pub fn begin_close_file(
        &mut self,
        file: FileHandle,
        flags: u32,
    ) -> Result<OperationHandle, Error> {
        // SAFETY: the current callback context remains live for this call.
        let result = unsafe { ffi::pb_file_close_begin(self.raw.as_ptr(), file, flags) };
        super::pending::admitted(result)
    }
    pub fn take_close_file(
        &mut self,
        operation: OperationHandle,
    ) -> Result<Completion<FileCloseResult>, Error> {
        let mut output = FileCloseResult::default();
        // SAFETY: the fixed terminal receipt is an exclusive initialized output.
        let code = unsafe { ffi::pb_file_close_take(self.raw.as_ptr(), operation, &mut output) };
        if code == Error::WouldBlock as i64 {
            return Ok(Completion::Pending);
        }
        status(code)?;
        Ok(Completion::Ready(output))
    }
    pub fn begin_file_list(&mut self, directory: FileHandle) -> Result<OperationHandle, Error> {
        // SAFETY: the current callback context remains live for this call.
        let result = unsafe { ffi::pb_file_list_open_begin(self.raw.as_ptr(), directory, 0) };
        super::pending::admitted(result)
    }
    pub fn take_file_list(
        &mut self,
        operation: OperationHandle,
    ) -> Result<Completion<FileListHandle>, Error> {
        // SAFETY: the current callback context remains live for this call.
        let code = unsafe { ffi::pb_file_list_open_take(self.raw.as_ptr(), operation) };
        if code == Error::WouldBlock as i64 {
            return Ok(Completion::Pending);
        }
        let output = FileListHandle(super::pending::handle_result(code)?);
        Ok(Completion::Ready(output))
    }
    pub fn begin_file_list_next(&mut self, list: FileListHandle) -> Result<OperationHandle, Error> {
        // SAFETY: the current callback context remains live for this call.
        let result = unsafe { ffi::pb_file_list_next_begin(self.raw.as_ptr(), list) };
        super::pending::admitted(result)
    }
    pub fn take_file_list_next<'a>(
        &mut self,
        operation: OperationHandle,
        storage: &'a mut [u8],
    ) -> Result<Completion<Option<FileListEntry<'a>>>, PollError> {
        let capacity = storage.len().min(255);
        let mut output = ffi::FileListEntry {
            size: core::mem::size_of::<ffi::FileListEntry>() as u32,
            written: 0,
            outcome: 0,
            kind: 0,
            name_data: storage.as_mut_ptr(),
            name_capacity: capacity as u32,
            name_count: 0,
        };
        // SAFETY: descriptor and payload are disjoint exclusive borrows.
        let code =
            unsafe { ffi::pb_file_list_next_take(self.raw.as_ptr(), operation, &mut output) };
        if code == Error::WouldBlock as i64 {
            return Ok(Completion::Pending);
        }
        if code == Error::BufferTooSmall as i64 {
            return Err(PollError::BufferTooSmall(output.name_count as usize));
        }
        status(code).map_err(PollError::Host)?;
        match output.outcome {
            ffi::FILE_LIST_END => Ok(Completion::Ready(None)),
            ffi::FILE_LIST_ENTRY => {
                let name = storage
                    .get(..output.name_count as usize)
                    .ok_or(PollError::Host(Error::Internal))?;
                Ok(Completion::Ready(Some(FileListEntry { kind: output.kind, name })))
            }
            _ => Err(PollError::Host(Error::Internal)),
        }
    }
    pub fn close_file_list(&mut self, list: FileListHandle) -> Result<(), Error> {
        // SAFETY: only the current callback context is borrowed.
        status(unsafe { ffi::pb_file_list_close(self.raw.as_ptr(), list) })
    }
    pub fn discard_file(&mut self, file: FileHandle) -> Result<(), Error> {
        // SAFETY: disposal borrows no output or application pointer.
        status(unsafe { ffi::pb_file_dispose(self.raw.as_ptr(), file) })
    }
    pub fn dispose_operation(&mut self, operation: OperationHandle) -> Result<(), Error> {
        // SAFETY: disposal borrows no output or application pointer.
        status(unsafe { ffi::pb_operation_dispose(self.raw.as_ptr(), operation) })
    }
}
