//! A trusted tool establishes one explicit context and never replays an exchange.
//! Blocking transport runs on the CLI/tool thread, not a guest or UI worker.
use super::{
    Request, Result,
    reply::Reply,
    service_call::{Context, ToolScope},
};
use crate::runtime::host::{DataCall, DataResult};

/// Preserve the typed service failure so observers can distinguish a retired
/// read from a denial. This never authorizes replay of an uncertain mutation.
#[derive(Debug)]
pub struct DataError(pub playbit_api::abi::syscall::Error);

impl std::fmt::Display for DataError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "Tool data operation failed: {:?}", self.0)
    }
}

impl std::error::Error for DataError {}

pub struct DataContext<'a> {
    call: &'a dyn Fn(Request) -> Result<Reply>,
    context: Context,
    operation: u64,
    mutation: u64,
    available: bool,
}

impl<'a> DataContext<'a> {
    pub fn open(
        session: Option<String>,
        call: &'a dyn Fn(Request) -> Result<Reply>,
    ) -> Result<Self> {
        Self::open_scope(session.map_or(ToolScope::Account, ToolScope::Session), call)
    }
    pub fn open_volume(
        volume: crate::store::Id,
        call: &'a dyn Fn(Request) -> Result<Reply>,
    ) -> Result<Self> {
        Self::open_scope(ToolScope::Volume(volume), call)
    }
    #[cfg(feature = "desktop")]
    pub(crate) fn open_browser(
        volume: crate::store::Id,
        session: Option<[u8; 32]>,
        call: &'a dyn Fn(Request) -> Result<Reply>,
    ) -> Result<Self> {
        let scope = session.map_or(ToolScope::BrowserVolume(volume), |id| {
            ToolScope::Session(crate::identifier::encode(&id))
        });
        Self::open_scope(scope, call)
    }
    fn open_scope(scope: ToolScope, call: &'a dyn Fn(Request) -> Result<Reply>) -> Result<Self> {
        let Reply::ToolContext { context: context @ Context::Tool { .. } } =
            call(Request::ToolContextOpen { scope })?
        else {
            return Err("Unexpected tool context admission reply".into());
        };
        Ok(Self { call, context, operation: 0, mutation: 0, available: true })
    }
    pub fn execute(&mut self, call: DataCall) -> Result<DataResult> {
        if !self.available {
            return Err(
                "Tool context lost its exchange; accepted mutations must not be replayed".into()
            );
        }
        self.operation = self.operation.checked_add(1).ok_or("Tool operation ID exhausted")?;
        let mutation = if call.operation.is_mutation() {
            self.mutation = self.mutation.checked_add(1).ok_or("Tool mutation ID exhausted")?;
            self.mutation
        } else {
            0
        };
        // Uncertainty freezes this client before transport starts. A complete
        // correlated outcome can restore it, including a contained denial.
        self.available = false;
        let Reply::Syscall(outcome) = (self.call)(Request::Syscall {
            context: self.context,
            operation: self.operation,
            mutation,
            call: Box::new(call.clone()),
        })?
        else {
            return Err(
                "Unexpected tool data reply; accepted mutations must not be replayed".into()
            );
        };
        let result = outcome.take(self.context, self.operation, mutation, &call);
        self.available = !matches!(
            result,
            Err(playbit_api::abi::syscall::Error::Internal
                | playbit_api::abi::syscall::Error::StaleHandle)
        );
        result.map_err(|error| DataError(error).into())
    }
    pub fn close(self) -> Result<()> {
        match (self.call)(Request::ToolContextClose { context: self.context })? {
            Reply::ToolContextClosed { context } if context == self.context => Ok(()),
            _ => Err("Unexpected tool context close reply".into()),
        }
    }
    /// Retire this context after success or failure. Cleanup has its own error
    /// owner and cannot obscure an acknowledged write or encourage a replay.
    pub fn finish<T>(self, result: Result<T>) -> Result<T> {
        if let Err(error) = self.close() {
            crate::diagnostic!("Tool context cleanup failed: {error}");
        }
        result
    }
    /// Stream bounded chunks; publish local output only after the caller accepts
    /// this final digest check. A concurrent field change never yields mixed bytes.
    pub fn read_file(
        &mut self,
        volume: [u8; 16],
        entry_id: [u8; 15],
        output: &mut dyn std::io::Write,
    ) -> Result<u64> {
        use crate::runtime::host::DataOperation;
        let DataResult::FileMetadata { len_bytes: total, digest } =
            self.execute(DataCall { volume, operation: DataOperation::FileMetadata { entry_id } })?
        else {
            return Err("Unexpected independent-file metadata result".into());
        };
        let mut hash = blake3::Hasher::new();
        let mut offset = 0u64;
        while offset < total {
            let DataResult::File(chunk) = self.execute(DataCall {
                volume,
                operation: DataOperation::FileRead {
                    entry_id,
                    offset,
                    limit: crate::store::BLOB_CHUNK_BYTES as u32,
                },
            })?
            else {
                return Err("Unexpected file chunk result".into());
            };
            if chunk.total_bytes != total || chunk.bytes.is_empty() {
                return Err("File changed during read; output was not published".into());
            }
            hash.update(&chunk.bytes);
            output.write_all(&chunk.bytes)?;
            offset =
                offset.checked_add(chunk.bytes.len() as u64).ok_or("File read length overflow")?;
        }
        if offset != total || *hash.finalize().as_bytes() != digest {
            return Err("File changed during read; output was not published".into());
        }
        Ok(total)
    }
    /// Staging is ephemeral and bounded. Only one final write commits the entry;
    /// a failed/lost chunk or commit never resumes or retries this transfer.
    pub fn write_file(
        &mut self,
        volume: [u8; 16],
        entry_id: [u8; 15],
        total: u64,
        input: &mut dyn std::io::Read,
    ) -> Result<()> {
        use crate::runtime::host::DataOperation;
        if total > super::blob::MAX_FILE_BYTES {
            return Err("file exceeds development transfer limit".into());
        }
        let DataResult::FileTransfer { transfer } = self.execute(DataCall {
            volume,
            operation: DataOperation::FileTransferStart { entry_id, total_bytes: total },
        })?
        else {
            return Err("Unexpected file transfer result".into());
        };
        let mut offset = 0u64;
        while offset < total {
            let length = (total - offset).min(super::tool_file::MAX_CHUNK_BYTES as u64) as usize;
            let mut bytes = vec![0; length];
            input.read_exact(&mut bytes)?;
            let DataResult::FileTransferred { offset: end } = self.execute(DataCall {
                volume,
                operation: DataOperation::FileTransferChunk { transfer, offset, bytes },
            })?
            else {
                return Err("Unexpected file transfer chunk result".into());
            };
            offset = end;
        }
        if input.read(&mut [0u8; 1])? != 0 {
            return Err("File grew during transfer; write was not committed".into());
        }
        let DataResult::Written = self.execute(DataCall {
            volume,
            operation: DataOperation::FileTransferCommit { transfer },
        })?
        else {
            return Err("Unexpected file write result".into());
        };
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Tests assert exact tool exchange behavior.")]
mod tests {
    use super::*;
    use std::cell::Cell;
    #[test]
    fn a_retired_read_preserves_its_typed_error_without_replaying_the_exchange() {
        use crate::{engine::service_call::Outcome, runtime::host::DataOperation, store::Id};
        let id = Id::from_bytes([4; 16]);
        let context = Context::Tool { handle: id, incarnation: id };
        let calls = Cell::new(0);
        let exchange = |request| match request {
            Request::ToolContextOpen { .. } => Ok(Reply::ToolContext { context }),
            Request::Syscall { context, operation, mutation, call } => {
                calls.set(calls.get() + 1);
                assert_eq!((operation, mutation), (1, 0));
                Ok(Reply::Syscall(Outcome {
                    context,
                    volume: Id::from_bytes(call.volume),
                    operation,
                    mutation,
                    kind: call.operation.kind() as u32,
                    revision: None,
                    status: playbit_api::abi::syscall::Error::Unavailable as i32,
                    result: None,
                }))
            }
            Request::ToolContextClose { context } => Ok(Reply::ToolContextClosed { context }),
            _ => panic!("unexpected exchange"),
        };
        let mut client = DataContext::open(None, &exchange).unwrap();
        let error = client
            .execute(DataCall {
                volume: [3; 16],
                operation: DataOperation::ResolveEntry { name: "data".into() },
            })
            .unwrap_err();
        assert_eq!(
            error.downcast_ref::<DataError>().unwrap().0,
            playbit_api::abi::syscall::Error::Unavailable
        );
        assert_eq!(calls.get(), 1);
        client.close().unwrap();
    }

    #[test]
    fn failed_cleanup_preserves_known_success_and_the_original_uncertain_failure() {
        // Each invocation opens, executes and closes exactly once. Cleanup
        // failure must not replace a received receipt or an uncertain exchange.
        use crate::{engine::service_call::Outcome, runtime::host::DataOperation, store::Id};
        for lose_reply in [false, true] {
            let id = Id::from_bytes([4; 16]);
            let context = Context::Tool { handle: id, incarnation: id };
            let (opens, writes, closes) = (Cell::new(0), Cell::new(0), Cell::new(0));
            let exchange = |request| match request {
                Request::ToolContextOpen { .. } => {
                    opens.set(opens.get() + 1);
                    Ok(Reply::ToolContext { context })
                }
                Request::Syscall { context, operation, mutation, call } => {
                    writes.set(writes.get() + 1);
                    if lose_reply {
                        return Err("write acknowledgment lost".into());
                    }
                    Ok(Reply::Syscall(Outcome {
                        context,
                        volume: Id::from_bytes(call.volume),
                        operation,
                        mutation,
                        kind: call.operation.kind() as u32,
                        revision: Some((id, 1)),
                        status: 0,
                        result: Some(DataResult::Created([5; 16])),
                    }))
                }
                Request::ToolContextClose { .. } => {
                    closes.set(closes.get() + 1);
                    Err("cleanup acknowledgment lost".into())
                }
                _ => panic!("unexpected exchange"),
            };
            let mut client = DataContext::open(None, &exchange).unwrap();
            let result = client.execute(DataCall {
                volume: [3; 16],
                operation: DataOperation::Create {
                    entry_id: [5; 15],
                    type_id: [2; 16],
                    fields: vec![],
                },
            });
            let result = client.finish(result);
            if lose_reply {
                assert_eq!(result.unwrap_err().to_string(), "write acknowledgment lost");
            } else {
                assert_eq!(result.unwrap(), DataResult::Created([5; 16]));
            }
            assert_eq!((opens.get(), writes.get(), closes.get()), (1, 1, 1));
        }
    }
    #[test]
    fn a_lost_bloblog_append_never_reopens_or_replays_its_binding() {
        let volume = crate::store::Id::from_bytes([2; 16]);
        let context = Context::Tool { handle: volume, incarnation: volume };
        let opens = Cell::new(0);
        let appends = Cell::new(0);
        let exchange = |request| match request {
            Request::ToolContextOpen { scope: ToolScope::Volume(target) } => {
                assert_eq!(target, volume);
                opens.set(opens.get() + 1);
                Ok(Reply::ToolContext { context })
            }
            Request::Syscall { call, .. } => {
                assert!(matches!(
                    call.operation,
                    crate::runtime::host::DataOperation::BloblogAppend { .. }
                ));
                appends.set(appends.get() + 1);
                Err("Append reply lost after possible commit".into())
            }
            Request::ToolContextClose { context } => Ok(Reply::ToolContextClosed { context }),
            _ => panic!("unexpected bloblog exchange"),
        };
        let mut client = DataContext::open_volume(volume, &exchange).unwrap();
        let call = DataCall {
            volume: *volume.as_bytes(),
            operation: crate::runtime::host::DataOperation::BloblogAppend {
                append_id: [1; 16],
                log: [3; 15],
                blobs: vec![vec![]],
            },
        };
        assert!(client.execute(call.clone()).is_err());
        assert!(client.execute(call).is_err());
        assert_eq!((opens.get(), appends.get()), (1, 1));
        client.close().unwrap();
    }
    #[test]
    fn lost_mutation_reply_freezes_new_calls_without_replay() {
        // One uncertain exchange cannot authorize a retry or a later data call.
        // Explicit context teardown remains available and does not undo storage.
        let handle = crate::store::Id::from_bytes([1; 16]);
        let context = Context::Tool { handle, incarnation: handle };
        let exchanges = Cell::new(0);
        let call = |request| match request {
            Request::ToolContextOpen { .. } => Ok(Reply::ToolContext { context }),
            Request::Syscall { .. } => {
                exchanges.set(exchanges.get() + 1);
                Err("Reply was lost after possible commit".into())
            }
            Request::ToolContextClose { context } => Ok(Reply::ToolContextClosed { context }),
            _ => panic!("unexpected tool request"),
        };
        let mut client = DataContext::open(None, &call).unwrap();
        let input = DataCall {
            volume: [2; 16],
            operation: crate::runtime::host::DataOperation::Create {
                entry_id: [5; 15],
                type_id: [3; 16],
                fields: vec![],
            },
        };
        assert!(client.execute(input.clone()).is_err());
        assert!(client.execute(input).is_err());
        assert_eq!(exchanges.get(), 1);
        client.close().unwrap();
    }
    #[test]
    fn a_lost_transfer_reply_never_replays_a_chunk_or_commit() {
        use crate::{engine::service_call::Outcome, runtime::host::DataOperation as O, store::Id};
        use std::cell::RefCell;
        for lose_commit in [false, true] {
            let handle = Id::from_bytes([1; 16]);
            let context = Context::Tool { handle, incarnation: handle };
            let sent = RefCell::new(Vec::new());
            let call = |request| match request {
                Request::ToolContextOpen { .. } => Ok(Reply::ToolContext { context }),
                Request::Syscall { context, operation, mutation, call } => {
                    sent.borrow_mut().push(call.operation.kind());
                    let result = match &call.operation {
                        O::FileTransferStart { .. } => {
                            DataResult::FileTransfer { transfer: [2; 16] }
                        }
                        O::FileTransferChunk { .. } if !lose_commit => {
                            return Err("Chunk reply lost".into());
                        }
                        O::FileTransferChunk { offset, bytes, .. } => {
                            DataResult::FileTransferred { offset: offset + bytes.len() as u64 }
                        }
                        O::FileTransferCommit { .. } => {
                            return Err("Commit reply lost after possible durable write".into());
                        }
                        _ => panic!("unexpected file operation"),
                    };
                    Ok(Reply::Syscall(Outcome {
                        context,
                        volume: Id::from_bytes(call.volume),
                        operation,
                        mutation,
                        kind: call.operation.kind() as u32,
                        revision: Some((handle, 1)),
                        status: 0,
                        result: Some(result),
                    }))
                }
                Request::ToolContextClose { context } => Ok(Reply::ToolContextClosed { context }),
                _ => panic!("unexpected tool request"),
            };
            let mut client = DataContext::open(None, &call).unwrap();
            assert!(client.write_file([3; 16], [4; 15], 3, &mut b"abc".as_slice()).is_err());
            let before = sent.borrow().clone();
            assert!(client.write_file([3; 16], [4; 15], 3, &mut b"abc".as_slice()).is_err());
            assert_eq!(*sent.borrow(), before);
            assert_eq!(before.len(), if lose_commit { 3 } else { 2 });
            client.close().unwrap();
        }
    }
}
