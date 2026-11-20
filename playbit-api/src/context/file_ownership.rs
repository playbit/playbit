//! Owned resources retain cleanup instructions, never a callback context pointer.
//! The application drains cleanup on its next callback; instance teardown owns
//! the final fallback. An implicit File drop always discards private contents.
extern crate alloc;
use super::{
    Completion, Context, Error, FileCloseResult, FileHandle, FileListEntry, FileListHandle,
    FileStatus, OperationHandle, PollError,
};
use alloc::{collections::VecDeque, rc::Rc};
use core::cell::RefCell;

// Runtime ceilings: 512 Files, 8 lists and 64 pending operations. Completed
// operations release their SDK slot even if the Rust wrapper is retained.
const MAX_OWNED: usize = 512 + 8 + 64;
#[derive(Clone, Copy)]
enum Cleanup {
    File(FileHandle),
    List(FileListHandle),
    Operation(OperationHandle),
}
struct State {
    live: usize,
    pending: VecDeque<Cleanup>,
}
/// One execution's resource owner. `App` attaches this automatically on each
/// callback. Embedders using `Context::from_raw` keep one owner and attach it
/// to each callback with `Context::attach_files`.
#[derive(Clone)]
pub struct FileResources(Rc<RefCell<State>>);
impl Default for FileResources {
    fn default() -> Self {
        Self(Rc::new(RefCell::new(State { live: 0, pending: VecDeque::with_capacity(MAX_OWNED) })))
    }
}
impl FileResources {
    fn reserve(&self) -> Result<Token, Error> {
        let mut state = self.0.borrow_mut();
        if state.live + state.pending.len() == MAX_OWNED {
            return Err(Error::LimitExceeded);
        }
        state.live += 1;
        Ok(Token { owner: self.clone(), cleanup: None })
    }
    pub(super) fn flush(&self, context: &mut Context<'_>) -> Result<(), Error> {
        loop {
            let Some(cleanup) = self.0.borrow().pending.front().copied() else { return Ok(()) };
            let result = match cleanup {
                Cleanup::File(file) => context.discard_file(file),
                Cleanup::List(list) => context.close_file_list(list),
                Cleanup::Operation(operation) => context.dispose_operation(operation),
            };
            match result {
                Ok(()) | Err(Error::Closed | Error::StaleHandle) => {
                    self.0.borrow_mut().pending.pop_front();
                }
                // A full event queue can delay a list-close notification. Keep
                // ownership; consuming existing events allows the next flush.
                Err(Error::LimitExceeded | Error::Busy | Error::WouldBlock) => return Ok(()),
                Err(error) => return Err(error),
            }
        }
    }
}
pub(super) struct Token {
    owner: FileResources,
    cleanup: Option<Cleanup>,
}
impl Token {
    fn check(&self, context: &Context<'_>) -> Result<(), Error> {
        if context.files.as_ref().is_some_and(|owner| Rc::ptr_eq(&owner.0, &self.owner.0)) {
            Ok(())
        } else {
            Err(Error::StaleHandle)
        }
    }
    fn operation(mut self, handle: OperationHandle, kind: OperationKind) -> FileOperation {
        self.cleanup = Some(Cleanup::Operation(handle));
        FileOperation { token: Some(self), handle, kind }
    }
}
impl Drop for Token {
    fn drop(&mut self) {
        let mut state = self.owner.0.borrow_mut();
        state.live -= 1;
        if let Some(cleanup) = self.cleanup {
            state.pending.push_back(cleanup);
        }
    }
}
impl Context<'_> {
    /// Attach the same owner on every callback of one execution. An owner must
    /// not be shared by separate runtime instances or reconstruction generations.
    pub fn attach_files(&mut self, owner: &FileResources) -> Result<(), Error> {
        if self.files.as_ref().is_some_and(|current| !Rc::ptr_eq(&current.0, &owner.0)) {
            return Err(Error::InvalidArgument);
        }
        self.files = Some(owner.clone());
        owner.flush(self)
    }
    /// Process destructor cleanup using this callback's live context.
    pub fn flush_files(&mut self) -> Result<(), Error> {
        let owner = self.files.clone().ok_or(Error::InvalidArgument)?;
        owner.flush(self)
    }
    fn file_token(&mut self) -> Result<Token, Error> {
        self.flush_files()?;
        self.files.as_ref().ok_or(Error::InvalidArgument)?.reserve()
    }
    pub(super) fn reserve_files(&mut self, count: usize) -> Result<alloc::vec::Vec<Token>, Error> {
        self.flush_files()?;
        let owner = self.files.as_ref().ok_or(Error::InvalidArgument)?;
        let state = owner.0.borrow();
        if count > MAX_OWNED - state.live - state.pending.len() {
            return Err(Error::LimitExceeded);
        }
        drop(state);
        (0..count).map(|_| owner.reserve()).collect()
    }
}
/// A directory, immutable reader, private writer, or recovery File. This value
/// is deliberately not Clone. Dropping it discards any unpublished contents.
pub struct File {
    token: Option<Token>,
    handle: FileHandle,
}
impl File {
    pub(super) fn adopt(mut token: Token, handle: FileHandle) -> Self {
        token.cleanup = Some(Cleanup::File(handle));
        Self { token: Some(token), handle }
    }
    fn check(&self, context: &Context<'_>) -> Result<(), Error> {
        self.token.as_ref().ok_or(Error::Closed)?.check(context)
    }
    pub fn open(
        &self,
        context: &mut Context<'_>,
        path: &[u8],
        mode: u32,
    ) -> Result<FileOperation, Error> {
        self.check(context)?;
        let token = context.file_token()?;
        Ok(token.operation(context.begin_open_file(self.handle, path, mode)?, OperationKind::Open))
    }
    pub fn read(
        &self,
        context: &mut Context<'_>,
        offset: u64,
        length: u32,
    ) -> Result<FileOperation, Error> {
        self.check(context)?;
        let token = context.file_token()?;
        Ok(token
            .operation(context.begin_read_file(self.handle, offset, length)?, OperationKind::Read))
    }
    pub fn write(
        &self,
        context: &mut Context<'_>,
        offset: u64,
        bytes: &[u8],
    ) -> Result<FileOperation, Error> {
        self.check(context)?;
        let token = context.file_token()?;
        Ok(token
            .operation(context.begin_write_file(self.handle, offset, bytes)?, OperationKind::Write))
    }
    pub fn status(&self, context: &mut Context<'_>) -> Result<FileOperation, Error> {
        self.check(context)?;
        let token = context.file_token()?;
        Ok(token.operation(context.begin_file_status(self.handle)?, OperationKind::Status))
    }
    /// Seal only after the host accepts Close. Failure leaves this File usable.
    pub fn close(&mut self, context: &mut Context<'_>, flags: u32) -> Result<FileOperation, Error> {
        self.check(context)?;
        let operation = context.begin_close_file(self.handle, flags)?;
        Ok(self.token.take().expect("checked live File").operation(operation, OperationKind::Close))
    }
    pub fn list(&self, context: &mut Context<'_>) -> Result<FileOperation, Error> {
        self.check(context)?;
        let token = context.file_token()?;
        Ok(token.operation(context.begin_file_list(self.handle)?, OperationKind::List))
    }
}
pub struct FileList {
    token: Option<Token>,
    handle: FileListHandle,
}
impl FileList {
    pub fn next(&self, context: &mut Context<'_>) -> Result<FileOperation, Error> {
        self.token.as_ref().ok_or(Error::Closed)?.check(context)?;
        let token = context.file_token()?;
        Ok(token.operation(context.begin_file_list_next(self.handle)?, OperationKind::Next))
    }
    pub fn close(&mut self, context: &mut Context<'_>) -> Result<(), Error> {
        self.token.as_ref().ok_or(Error::Closed)?.check(context)?;
        context.close_file_list(self.handle)?;
        if let Some(mut token) = self.token.take() {
            token.cleanup = None;
        }
        Ok(())
    }
}
#[derive(Clone, Copy, PartialEq)]
enum OperationKind {
    Open,
    Read,
    Write,
    Status,
    Close,
    List,
    Next,
}
/// An accepted operation owns its retained result until collected or dropped.
/// Drop suppresses delivery; it does not cancel an accepted write or Close.
pub struct FileOperation {
    token: Option<Token>,
    handle: OperationHandle,
    kind: OperationKind,
}
pub struct FileClose {
    pub error: Option<Error>,
    pub publication: u32,
    pub recovery: Option<File>,
}
impl FileOperation {
    pub fn handle(&self) -> OperationHandle {
        self.handle
    }
    fn check(&self, context: &Context<'_>, kind: OperationKind) -> Result<(), Error> {
        if self.kind != kind {
            return Err(Error::InvalidArgument);
        }
        self.token.as_ref().ok_or(Error::Closed)?.check(context)
    }
    fn finish(&mut self) {
        if let Some(mut token) = self.token.take() {
            token.cleanup = None;
        }
    }
    fn plain<T>(&mut self, result: Result<Completion<T>, Error>) -> Result<Completion<T>, Error> {
        if !matches!(result, Ok(Completion::Pending)) {
            self.finish();
        }
        result
    }
    fn buffered<T>(
        &mut self,
        result: Result<Completion<T>, PollError>,
    ) -> Result<Completion<T>, PollError> {
        if !matches!(result, Ok(Completion::Pending) | Err(PollError::BufferTooSmall(_))) {
            self.finish();
        }
        result
    }
    pub fn take_open(&mut self, context: &mut Context<'_>) -> Result<Completion<File>, Error> {
        self.check(context, OperationKind::Open)?;
        match context.take_open_file(self.handle) {
            Ok(Completion::Pending) => Ok(Completion::Pending),
            Ok(Completion::Ready(handle)) => Ok(Completion::Ready(File::adopt(
                self.token.take().expect("live operation"),
                handle,
            ))),
            Err(error) => {
                self.finish();
                Err(error)
            }
        }
    }
    pub fn take_read<'a>(
        &mut self,
        context: &mut Context<'_>,
        storage: &'a mut [u8],
    ) -> Result<Completion<&'a [u8]>, PollError> {
        self.check(context, OperationKind::Read).map_err(PollError::Host)?;
        let result = context.take_read_file(self.handle, storage);
        self.buffered(result)
    }
    pub fn take_write(&mut self, context: &mut Context<'_>) -> Result<Completion<()>, Error> {
        self.check(context, OperationKind::Write)?;
        self.plain(context.take_write_file(self.handle))
    }
    pub fn take_status(
        &mut self,
        context: &mut Context<'_>,
    ) -> Result<Completion<FileStatus>, Error> {
        self.check(context, OperationKind::Status)?;
        self.plain(context.take_file_status(self.handle))
    }
    pub fn take_close(
        &mut self,
        context: &mut Context<'_>,
    ) -> Result<Completion<FileClose>, Error> {
        self.check(context, OperationKind::Close)?;
        match context.take_close_file(self.handle) {
            Ok(Completion::Pending) => Ok(Completion::Pending),
            Ok(Completion::Ready(result)) => {
                let FileCloseResult { error, publication, .. } = result;
                let recovery = if result.recovery().slot() != 0 {
                    Some(File::adopt(self.token.take().expect("live operation"), result.recovery()))
                } else {
                    self.finish();
                    None
                };
                Ok(Completion::Ready(FileClose {
                    error: (error != 0).then(|| Error::try_from(error).unwrap_or(Error::Internal)),
                    publication,
                    recovery,
                }))
            }
            Err(error) => {
                self.finish();
                Err(error)
            }
        }
    }
    pub fn take_list(&mut self, context: &mut Context<'_>) -> Result<Completion<FileList>, Error> {
        self.check(context, OperationKind::List)?;
        match context.take_file_list(self.handle) {
            Ok(Completion::Pending) => Ok(Completion::Pending),
            Ok(Completion::Ready(handle)) => {
                let mut token = self.token.take().expect("live operation");
                token.cleanup = Some(Cleanup::List(handle));
                Ok(Completion::Ready(FileList { token: Some(token), handle }))
            }
            Err(error) => {
                self.finish();
                Err(error)
            }
        }
    }
    pub fn take_next<'a>(
        &mut self,
        context: &mut Context<'_>,
        storage: &'a mut [u8],
    ) -> Result<Completion<Option<FileListEntry<'a>>>, PollError> {
        self.check(context, OperationKind::Next).map_err(PollError::Host)?;
        let result = context.take_file_list_next(self.handle, storage);
        self.buffered(result)
    }
}
